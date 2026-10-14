use tokio::io::AsyncReadExt;

use crate::util::allocate_vec;

/// Buffered async I/O reader; kept separate from the I/O-independent `SlideBuffer`.
pub struct StreamReader {
    buf: Box<[u8]>,
    start_offset: usize,
    end_offset: usize,
}

impl StreamReader {
    pub fn new_with_buffer_size(buffer_size: usize) -> Self {
        // The buffer_size also determines the maximum line length that can be read.
        Self {
            buf: allocate_vec(buffer_size).into_boxed_slice(),
            start_offset: 0usize,
            end_offset: 0usize,
        }
    }

    fn reset_buf_offset(&mut self) {
        if self.start_offset == 0 {
            return;
        }
        self.buf.copy_within(self.start_offset..self.end_offset, 0);
        self.end_offset -= self.start_offset;
        self.start_offset = 0;
    }

    pub async fn read_u8<T: AsyncReadExt + Unpin>(
        &mut self,
        stream: &mut T,
    ) -> std::io::Result<u8> {
        while self.end_offset - self.start_offset < 1 {
            self.read(stream).await?;
        }
        let value = self.buf[self.start_offset];
        let new_start_offset = self.start_offset + 1;
        if new_start_offset == self.end_offset {
            self.start_offset = 0;
            self.end_offset = 0;
        } else {
            self.start_offset = new_start_offset;
        }
        Ok(value)
    }

    /// Peek at the first byte without consuming it.
    /// Ensures at least 1 byte is buffered, then returns it.
    pub async fn peek_u8<T: AsyncReadExt + Unpin>(
        &mut self,
        stream: &mut T,
    ) -> std::io::Result<u8> {
        while self.end_offset - self.start_offset < 1 {
            self.read(stream).await?;
        }
        // Returns the byte without advancing start_offset.
        Ok(self.buf[self.start_offset])
    }

    /// Peek at the first `len` bytes without consuming them.
    /// Ensures at least `len` bytes are buffered, then returns a reference to them.
    pub async fn peek_slice<T: AsyncReadExt + Unpin + ?Sized>(
        &mut self,
        stream: &mut T,
        len: usize,
    ) -> std::io::Result<&[u8]> {
        if len > self.buf.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "Requested length {} exceeds buffer size {}",
                    len,
                    self.buf.len()
                ),
            ));
        }
        while self.end_offset - self.start_offset < len {
            self.read(stream).await?;
        }
        // Returns the slice without advancing start_offset.
        Ok(&self.buf[self.start_offset..self.start_offset + len])
    }

    /// Consume (skip) `len` bytes that were previously peeked.
    /// This advances the read position without returning any data.
    pub fn consume(&mut self, len: usize) {
        let new_start_offset = self.start_offset + len;
        debug_assert!(new_start_offset <= self.end_offset);
        if new_start_offset == self.end_offset {
            self.start_offset = 0;
            self.end_offset = 0;
        } else {
            self.start_offset = new_start_offset;
        }
    }

    pub async fn read_u16_be<T: AsyncReadExt + Unpin>(
        &mut self,
        stream: &mut T,
    ) -> std::io::Result<u16> {
        while self.end_offset - self.start_offset < 2 {
            self.read(stream).await?;
        }
        let value =
            u16::from_be_bytes([self.buf[self.start_offset], self.buf[self.start_offset + 1]]);
        let new_start_offset = self.start_offset + 2;
        if new_start_offset == self.end_offset {
            self.start_offset = 0;
            self.end_offset = 0;
        } else {
            self.start_offset = new_start_offset;
        }
        Ok(value)
    }

    pub async fn read_slice<T: AsyncReadExt + Unpin + ?Sized>(
        &mut self,
        stream: &mut T,
        len: usize,
    ) -> std::io::Result<&[u8]> {
        if len > self.buf.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "Requested length {} exceeds buffer size {}",
                    len,
                    self.buf.len()
                ),
            ));
        }
        while self.end_offset - self.start_offset < len {
            self.read(stream).await?;
        }
        let slice = &self.buf[self.start_offset..self.start_offset + len];
        let new_start_offset = self.start_offset + len;
        if new_start_offset == self.end_offset {
            self.start_offset = 0;
            self.end_offset = 0;
        } else {
            self.start_offset = new_start_offset;
        }
        Ok(slice)
    }

    pub fn unparsed_data(&self) -> &[u8] {
        &self.buf[self.start_offset..self.end_offset]
    }

    pub fn unparsed_data_owned(&self) -> Option<Box<[u8]>> {
        let unparsed_data = self.unparsed_data();
        if unparsed_data.is_empty() {
            None
        } else {
            Some(unparsed_data.to_vec().into_boxed_slice())
        }
    }

    async fn read<T: AsyncReadExt + Unpin + ?Sized>(
        &mut self,
        stream: &mut T,
    ) -> std::io::Result<()> {
        // Returns immediately after a single read() call to support blocking I/O.
        if self.is_cache_full() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "cache is full",
            ));
        }

        // Clears the offset to make space for the next read.
        self.reset_buf_offset();

        loop {
            match stream.read(&mut self.buf[self.end_offset..]).await {
                Ok(len) => {
                    if len == 0 {
                        // EOF
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionAborted,
                            "EOF while reading",
                        ));
                    }
                    self.end_offset += len;
                    return Ok(());
                }
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    } else {
                        return Err(e);
                    }
                }
            }
        }
    }

    fn is_cache_full(&self) -> bool {
        self.start_offset == 0 && self.end_offset == self.buf.len()
    }
}
