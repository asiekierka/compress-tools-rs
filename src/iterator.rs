use std::{
    ffi::{CStr, CString},
    io::{Read, Seek, SeekFrom, Write},
    ops::DerefMut,
    slice,
};

use libc::{c_int, c_void};

use crate::{error::archive_result, ffi, ffi::UTF8LocaleGuard, Error, Result, READER_BUFFER_SIZE};

struct HeapReadSeekerPipe<R: Read + Seek> {
    reader: R,
    buffer: [u8; READER_BUFFER_SIZE],
}

/// The contents of an archive, yielded in order from the beginning to the end
/// of the archive.
///
/// Each entry, file or directory, will have a
/// [`ArchiveContents::StartOfEntry`], zero or more
/// [`ArchiveContents::DataChunk`], and then a corresponding
/// [`ArchiveContents::EndOfEntry`] to mark that the entry has been read to
/// completion.
pub enum ArchiveContents {
    /// Marks the start of an entry, either a file or a directory.
    StartOfEntry(String),
    /// A chunk of uncompressed data from the entry. Entries may have zero or
    /// more chunks.
    DataChunk(Vec<u8>),
    /// Marks the end of the entry that was started by the previous
    /// StartOfEntry.
    EndOfEntry,
    Err(Error),
}

/// An iterator over the contents of an archive.
pub struct ArchiveIterator<R: Read + Seek> {
    archive_entry: *mut ffi::archive_entry,
    archive_reader: *mut ffi::archive,

    in_file: bool,
    closed: bool,
    error: bool,

    _pipe: Box<HeapReadSeekerPipe<R>>,
    _utf8_guard: UTF8LocaleGuard,
}

impl<R: Read + Seek> Iterator for ArchiveIterator<R> {
    type Item = ArchiveContents;

    fn next(&mut self) -> Option<Self::Item> {
        if self.error {
            return None;
        }

        let next = if self.in_file {
            unsafe { self.next_data_chunk() }
        } else {
            unsafe { self.next_header() }
        };

        match &next {
            ArchiveContents::StartOfEntry(_) => {
                self.in_file = true;
                Some(next)
            }
            ArchiveContents::DataChunk(_) => Some(next),
            ArchiveContents::EndOfEntry if self.in_file => {
                self.in_file = false;
                Some(next)
            }
            ArchiveContents::EndOfEntry => None,
            ArchiveContents::Err(_) => {
                self.error = true;
                Some(next)
            }
        }
    }
}

impl<R: Read + Seek> Drop for ArchiveIterator<R> {
    fn drop(&mut self) {
        drop(self.free());
    }
}

impl<R: Read + Seek> ArchiveIterator<R> {
    /// Iterate over the contents of an archive, streaming the contents of each
    /// entry in small chunks.
    ///
    /// ```no_run
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use compress_tools::*;
    /// use std::fs::File;
    ///
    /// let file = File::open("tree.tar")?;
    ///
    /// let mut name = String::default();
    /// let mut size = 0;
    /// let mut iter = ArchiveIterator::from_read(file)?;
    ///
    /// for content in &mut iter {
    ///     match content {
    ///         ArchiveContents::StartOfEntry(s) => name = s,
    ///         ArchiveContents::DataChunk(v) => size += v.len(),
    ///         ArchiveContents::EndOfEntry => {
    ///             println!("Entry {} was {} bytes", name, size);
    ///             size = 0;
    ///         }
    ///         ArchiveContents::Err(e) => {
    ///             Err(e)?;
    ///         }
    ///     }
    /// }
    ///
    /// iter.close()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_read(source: R) -> Result<ArchiveIterator<R>>
    where
        R: Read + Seek + 'static,
    {
        let utf8_guard = ffi::UTF8LocaleGuard::new();
        let reader = source;
        let buffer = [0; READER_BUFFER_SIZE];
        let mut pipe = Box::new(HeapReadSeekerPipe { reader, buffer });

        unsafe {
            let archive_entry: *mut ffi::archive_entry = std::ptr::null_mut();
            let archive_reader = ffi::archive_read_new();

            let res = (|| {
                archive_result(
                    ffi::archive_read_support_filter_all(archive_reader),
                    archive_reader,
                )?;

                archive_result(
                    ffi::archive_read_support_format_all(archive_reader),
                    archive_reader,
                )?;

                archive_result(
                    ffi::archive_read_set_seek_callback(
                        archive_reader,
                        Some(libarchive_heap_seek_callback::<R>),
                    ),
                    archive_reader,
                )?;

                if archive_reader.is_null() {
                    return Err(Error::NullArchive);
                }

                archive_result(
                    ffi::archive_read_open(
                        archive_reader,
                        (pipe.deref_mut() as *mut HeapReadSeekerPipe<R>) as *mut c_void,
                        None,
                        Some(libarchive_heap_seekableread_callback::<R>),
                        None,
                    ),
                    archive_reader,
                )?;

                Ok(())
            })();

            let iter = ArchiveIterator {
                archive_entry,
                archive_reader,

                in_file: false,
                closed: false,
                error: false,

                _pipe: pipe,
                _utf8_guard: utf8_guard,
            };

            res?;
            Ok(iter)
        }
    }

    /// Close the iterator, freeing up the associated resources.
    ///
    /// Resources will be freed on drop if this is not called, but any errors
    /// during freeing on drop will be lost.
    pub fn close(mut self) -> Result<()> {
        self.free()
    }

    fn free(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }

        self.closed = true;
        unsafe {
            archive_result(
                ffi::archive_read_close(self.archive_reader),
                self.archive_reader,
            )?;
            archive_result(
                ffi::archive_read_free(self.archive_reader),
                self.archive_reader,
            )?;
        }
        Ok(())
    }

    unsafe fn next_header(&mut self) -> ArchiveContents {
        match ffi::archive_read_next_header(self.archive_reader, &mut self.archive_entry) {
            ffi::ARCHIVE_EOF => ArchiveContents::EndOfEntry,
            ffi::ARCHIVE_OK => {
                let file_name = CStr::from_ptr(ffi::archive_entry_pathname(self.archive_entry))
                    .to_string_lossy()
                    .into_owned();
                ArchiveContents::StartOfEntry(file_name)
            }
            _ => ArchiveContents::Err(Error::from(self.archive_reader)),
        }
    }

    unsafe fn next_data_chunk(&mut self) -> ArchiveContents {
        let mut buffer = std::ptr::null();
        let mut offset = 0;
        let mut size = 0;
        let mut target = Vec::with_capacity(READER_BUFFER_SIZE);

        match ffi::archive_read_data_block(self.archive_reader, &mut buffer, &mut size, &mut offset)
        {
            ffi::ARCHIVE_EOF => ArchiveContents::EndOfEntry,
            ffi::ARCHIVE_OK => {
                let content = slice::from_raw_parts(buffer as *const u8, size);
                let write = target.write_all(content);
                if let Err(e) = write {
                    ArchiveContents::Err(e.into())
                } else {
                    ArchiveContents::DataChunk(target)
                }
            }
            _ => ArchiveContents::Err(Error::from(self.archive_reader)),
        }
    }
}

unsafe extern "C" fn libarchive_heap_seek_callback<R: Read + Seek>(
    _: *mut ffi::archive,
    client_data: *mut c_void,
    offset: ffi::la_int64_t,
    whence: c_int,
) -> i64 {
    let pipe = (client_data as *mut HeapReadSeekerPipe<R>)
        .as_mut()
        .unwrap();
    let whence = match whence {
        0 => SeekFrom::Start(offset as u64),
        1 => SeekFrom::Current(offset),
        2 => SeekFrom::End(offset),
        _ => return -1,
    };

    match pipe.reader.seek(whence) {
        Ok(offset) => offset as i64,
        Err(_) => -1,
    }
}

unsafe extern "C" fn libarchive_heap_seekableread_callback<R: Read + Seek>(
    archive: *mut ffi::archive,
    client_data: *mut c_void,
    buffer: *mut *const c_void,
) -> ffi::la_ssize_t {
    let pipe = (client_data as *mut HeapReadSeekerPipe<R>)
        .as_mut()
        .unwrap();

    *buffer = pipe.buffer.as_ptr() as *const c_void;

    match pipe.reader.read(&mut pipe.buffer) {
        Ok(size) => size as ffi::la_ssize_t,
        Err(e) => {
            let description = CString::new(e.to_string()).unwrap();

            ffi::archive_set_error(archive, e.raw_os_error().unwrap_or(0), description.as_ptr());

            -1
        }
    }
}
