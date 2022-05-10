//! Module contains a Session structure.

use std::{
    convert::TryInto,
    io::{self, BufRead, BufReader, Read, Write},
    ops::{Deref, DerefMut},
    time::{self, Duration},
};

use crate::{
    control_code::ControlCode,
    error::{to_io_error, Error},
    needle::Needle,
    process::NonBlocking,
    Captures,
};

/// Session represents a spawned process and its streams.
/// It controlls process and communication with it.
#[derive(Debug)]
pub struct Session<P, S> {
    proc: P,
    stream: TryStream<S>,
    expect_timeout: Option<Duration>,
}

impl<P, S: Read> Session<P, S> {
    pub(crate) fn new(process: P, stream: S) -> io::Result<Self> {
        let stream = TryStream::new(stream)?;
        Ok(Self {
            proc: process,
            stream,
            expect_timeout: Some(Duration::from_millis(10000)),
        })
    }

    pub(crate) fn swap_stream<F: FnOnce(S) -> R, R: Read>(
        mut self,
        new_stream: F,
    ) -> Result<Session<P, R>, Error> {
        self.stream.flush_in_buffer();
        let buf = self.stream.get_available().to_owned();

        let stream = self.stream.into_inner();
        let new_stream = new_stream(stream);

        let mut session = Session::new(self.proc, new_stream)?;
        session.stream.keep_in_buffer(&buf);
        Ok(session)
    }
}

impl<P, S> Session<P, S> {
    /// Set the pty session's expect timeout.
    pub fn set_expect_timeout(&mut self, expect_timeout: Option<Duration>) {
        self.expect_timeout = expect_timeout;
    }
}

impl<P, S: Read + NonBlocking> Session<P, S> {
    /// Expect waits until a pattern is matched.
    ///
    /// If the method returns [Ok] it is guaranteed that at least 1 match was found.
    ///
    /// This make assertions in a lazy manner. Starts from 1st byte then checks 2nd byte and goes further.
    /// It is done intentinally to be presize.
    /// Here's an example,
    /// when you call this method with [crate::Regex] and output contains 123, expect will return ‘1’ as a match not ‘123’.
    ///
    /// # Example
    ///
    /// ```
    /// let mut p = expectrl::spawn("echo 123").unwrap();
    /// let m = p.expect(expectrl::Regex("\\d+")).unwrap();
    /// assert_eq!(m.get(0).unwrap(), b"1");
    /// ```
    ///
    /// This behaviour is different from [Session::check].
    ///
    /// It returns an error if timeout is reached.
    /// You can specify a timeout value by [Session::set_expect_timeout] method.
    pub fn expect<E: Needle>(&mut self, expect: E) -> Result<Captures, Error> {
        let mut checking_data_length = 0;
        let mut eof = false;
        let start = time::Instant::now();
        loop {
            let mut available = self.stream.get_available();
            if checking_data_length == available.len() {
                // We read by byte to make things as lazy as possible.
                //
                // It's chose is important in using Regex as a Needle.
                // Imagine we have a `\d+` regex.
                // Using such buffer will match string `2` imidiately eventhough right after might be other digit.
                //
                // The second reason is
                // if we wouldn't read by byte EOF indication could be lost.
                // And next blocking std::io::Read operation could be blocked forever.
                //
                // We could read all data available via `read_available` to reduce IO operations,
                // but in such case we would need to keep a EOF indicator internally in stream,
                // which is OK if EOF happens onces, but I am not sure if this is a case.
                eof = self.stream.read_available_once(&mut [0; 1])? == Some(0);
                available = self.stream.get_available();
            }

            // We intentinally not increase the counter
            // and run check one more time even though the data isn't changed.
            // Because it may be important for custom implementations of Needle.
            if checking_data_length < available.len() {
                checking_data_length += 1;
            }

            let data = &available[..checking_data_length];

            let found = expect.check(data, eof)?;
            if !found.is_empty() {
                let end_index = Captures::right_most_index(&found);
                let involved_bytes = data[..end_index].to_vec();
                self.stream.consume_available(end_index);
                return Ok(Captures::new(involved_bytes, found));
            }

            if eof {
                return Err(Error::Eof);
            }

            if let Some(timeout) = self.expect_timeout {
                if start.elapsed() > timeout {
                    return Err(Error::ExpectTimeout);
                }
            }
        }
    }

    /// Check verifies if a pattern is matched.
    /// Returns empty found structure if nothing found.
    ///
    /// Is a non blocking version of [Session::expect].
    /// But its strategy of matching is different from it.
    /// It makes search against all bytes available.
    ///
    /// # Example
    ///
    /// ```
    /// use expectrl::{spawn, Regex};
    /// use std::time::Duration;
    ///
    /// let mut p = spawn("echo 123").unwrap();
    ///
    /// // wait to guarantee that check echo worked out (most likely)
    /// std::thread::sleep(Duration::from_secs(1));
    ///
    /// let m = p.check(Regex("\\d+")).unwrap();
    /// assert_eq!(m.get(0).unwrap(), b"123");
    /// ```
    pub fn check<E: Needle>(&mut self, needle: E) -> Result<Captures, Error> {
        let eof = self.stream.read_available()?;
        let buf = self.stream.get_available();

        let found = needle.check(buf, eof)?;
        if !found.is_empty() {
            let end_index = Captures::right_most_index(&found);
            let involved_bytes = buf[..end_index].to_vec();
            self.stream.consume_available(end_index);
            return Ok(Captures::new(involved_bytes, found));
        }

        if eof {
            return Err(Error::Eof);
        }

        Ok(Captures::new(Vec::new(), Vec::new()))
    }

    /// The functions checks if a pattern is matched.
    /// It doesn’t consumes bytes from stream.
    ///
    /// Its strategy of matching is different from the one in [Session::expect].
    /// It makes search agains all bytes available.
    ///
    /// If you want to get a matched result [Session::check] and [Session::expect] is a better option.
    /// Because it is not guaranteed that [Session::check] or [Session::expect] with the same parameters:
    ///     - will successed even right after Session::is_matched call.
    ///     - will operate on the same bytes.
    ///
    /// IMPORTANT:
    ///  
    /// If you call this method with [crate::Eof] pattern be aware that eof
    /// indication MAY be lost on the next interactions.
    /// It depends from a process you spawn.
    /// So it might be better to use [Session::check] or [Session::expect] with Eof.
    ///
    /// # Example
    ///
    /// ```
    /// use expectrl::{spawn, Regex};
    /// use std::time::Duration;
    ///
    /// let mut p = spawn("echo 123").unwrap();
    ///
    /// // wait to guarantee that check echo worked out (most likely)
    /// std::thread::sleep(Duration::from_secs(1));
    ///
    /// let m = p.is_matched(Regex("\\d+")).unwrap();
    /// assert_eq!(m, true);
    /// ```
    pub fn is_matched<E: Needle>(&mut self, needle: E) -> Result<bool, Error> {
        let eof = self.stream.read_available()?;
        let buf = self.stream.get_available();

        let found = needle.check(buf, eof)?;
        if !found.is_empty() {
            return Ok(true);
        }

        if eof {
            return Err(Error::Eof);
        }

        Ok(false)
    }
}

impl<P, S: Write> Session<P, S> {
    /// Send text to child’s STDIN.
    ///
    /// You can also use methods from [std::io::Write] instead.
    pub fn send(&mut self, s: impl AsRef<str>) -> io::Result<()> {
        self.stream.write_all(s.as_ref().as_bytes())
    }

    /// Send a line to child’s STDIN.
    pub fn send_line(&mut self, s: impl AsRef<str>) -> io::Result<()> {
        // fixme: move it to a processes stream function.
        #[cfg(windows)]
        {
            // win32 has writefilegather function which could be used as write_vectored but it asyncronos which may involve some issue?
            // https://docs.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-writefilegather

            const LINE_ENDING: &[u8] = b"\r\n";
            let _ = self.write_all(s.as_ref().as_bytes())?;
            let _ = self.write_all(LINE_ENDING)?;
            self.flush()?;
            Ok(())
        }
        #[cfg(not(windows))]
        {
            const LINE_ENDING: &[u8] = b"\n";

            let bufs = &mut [
                std::io::IoSlice::new(s.as_ref().as_bytes()),
                std::io::IoSlice::new(LINE_ENDING),
                std::io::IoSlice::new(&[]), // we need to add a empty one as it may be not written.
            ];

            // As Write trait says it's not guaranteed that write_vectored will write_all data.
            // But we are sure that write_vectored writes everyting or nothing because underthehood it uses a File.
            // But we rely on this fact not explicitely.
            //
            // todo: check amount of written bytes ands write the rest if not everyting was written already.
            let _ = self.write_vectored(bufs)?;
            self.flush()?;

            Ok(())
        }
    }

    /// Sends controll character to a child process.
    ///
    /// You must be carefull passing a char or &str as an argument.
    /// If you pass an unexpected controll you’ll get a error.
    /// So it may be better to use [ControlCode].
    ///
    /// # Example
    ///
    /// ```
    /// use expectrl::{spawn, ControlCode};
    ///
    /// let mut process = spawn("cat").unwrap();
    ///
    /// process.send_control(ControlCode::EndOfText); // sends CTRL^C
    /// process.send_control('C'); // sends CTRL^C
    /// process.send_control("^C"); // sends CTRL^C
    /// ```
    pub fn send_control(&mut self, code: impl TryInto<ControlCode>) -> io::Result<()> {
        let code = code
            .try_into()
            .map_err(|_| to_io_error("Failed to parse a control character")(""))?;
        self.stream.write_all(&[code.into()])
    }
}

impl<P, S: Read + NonBlocking> Session<P, S> {
    /// Try to read in a non-blocking mode.
    ///
    /// Returns `[std::io::ErrorKind::WouldBlock]`
    /// in case if there's nothing to read.
    pub fn try_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.try_read(buf)
    }

    /// Verifyes if stream is empty or not.
    pub fn is_empty(&mut self) -> io::Result<bool> {
        self.stream.is_empty()
    }
}

impl<P, S: Write> Write for Session<P, S> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.stream.write_vectored(bufs)
    }
}

impl<P, S: Read> Read for Session<P, S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.stream.read(buf)
    }
}

impl<P, S: Read> BufRead for Session<P, S> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.stream.fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        self.stream.consume(amt)
    }
}

impl<P, S> Deref for Session<P, S> {
    type Target = P;

    fn deref(&self) -> &Self::Target {
        &self.proc
    }
}

impl<P, S> DerefMut for Session<P, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.proc
    }
}

#[derive(Debug)]
struct TryStream<S> {
    stream: ControlledReader<S>,
}

impl<S> TryStream<S> {
    fn into_inner(self) -> S {
        self.stream.inner.into_inner().inner
    }
}

impl<S: Read> TryStream<S> {
    /// The function returns a new Stream from a file.
    fn new(stream: S) -> io::Result<Self> {
        Ok(Self {
            stream: ControlledReader::new(stream),
        })
    }

    fn flush_in_buffer(&mut self) {
        self.stream.flush_in_buffer();
    }
}

impl<S> TryStream<S> {
    fn keep_in_buffer(&mut self, v: &[u8]) {
        self.stream.keep_in_buffer(v);
    }

    fn get_available(&mut self) -> &[u8] {
        self.stream.get_available()
    }

    fn consume_available(&mut self, n: usize) {
        self.stream.consume_available(n)
    }
}

impl<R: Read + NonBlocking> TryStream<R> {
    /// Try to read in a non-blocking mode.
    ///
    /// It raises io::ErrorKind::WouldBlock if there's nothing to read.
    fn try_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.get_mut().set_non_blocking()?;

        let result = match self.stream.inner.read(buf) {
            Ok(n) => Ok(n),
            Err(err) => Err(err),
        };

        // As file is DUPed changes in one descriptor affects all ones
        // so we need to make blocking file after we finished.
        self.stream.get_mut().set_blocking()?;

        result
    }

    #[allow(clippy::wrong_self_convention)]
    fn is_empty(&mut self) -> io::Result<bool> {
        match self.try_read(&mut []) {
            Ok(0) => Ok(true),
            Ok(_) => Ok(false),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(true),
            Err(err) => Err(err),
        }
    }

    fn read_available(&mut self) -> std::io::Result<bool> {
        self.stream.flush_in_buffer();

        let mut buf = [0; 248];
        loop {
            match self.try_read_inner(&mut buf) {
                Ok(0) => break Ok(true),
                Ok(n) => {
                    self.stream.keep_in_buffer(&buf[..n]);
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break Ok(false),
                Err(err) => break Err(err),
            }
        }
    }

    fn read_available_once(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        self.stream.flush_in_buffer();

        match self.try_read_inner(buf) {
            Ok(0) => Ok(Some(0)),
            Ok(n) => {
                self.stream.keep_in_buffer(&buf[..n]);
                Ok(Some(n))
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(err) => Err(err),
        }
    }

    // non-buffered && non-blocking read
    fn try_read_inner(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.get_mut().set_non_blocking()?;

        let result = match self.stream.get_mut().read(buf) {
            Ok(n) => Ok(n),
            Err(err) => Err(err),
        };

        // As file is DUPed changes in one descriptor affects all ones
        // so we need to make blocking file after we finished.
        self.stream.get_mut().set_blocking()?;

        result
    }
}

impl<S: Write> Write for TryStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.inner.get_mut().inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.inner.get_mut().inner.flush()
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.stream.inner.get_mut().inner.write_vectored(bufs)
    }
}

impl<R: Read> Read for TryStream<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.inner.read(buf)
    }
}

impl<R: Read> BufRead for TryStream<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.stream.inner.fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        self.stream.inner.consume(amt)
    }
}

#[derive(Debug)]
struct ControlledReader<R> {
    inner: BufReader<BufferedReader<R>>,
}

impl<R: Read> ControlledReader<R> {
    fn new(reader: R) -> Self {
        Self {
            inner: BufReader::new(BufferedReader::new(reader)),
        }
    }

    fn flush_in_buffer(&mut self) {
        // Because we have 2 buffered streams there might appear inconsistancy
        // in read operations and the data which was via `keep_in_buffer` function.
        //
        // To eliminate it we move BufReader buffer to our buffer.
        let b = self.inner.buffer().to_vec();
        self.inner.consume(b.len());
        self.keep_in_buffer(&b);
    }
}

impl<R> ControlledReader<R> {
    fn keep_in_buffer(&mut self, v: &[u8]) {
        self.inner.get_mut().buffer.extend(v);
    }

    fn get_mut(&mut self) -> &mut R {
        &mut self.inner.get_mut().inner
    }

    fn get_available(&mut self) -> &[u8] {
        &self.inner.get_ref().buffer
    }

    fn consume_available(&mut self, n: usize) {
        let _ = self.inner.get_mut().buffer.drain(..n);
    }
}

#[derive(Debug)]
struct BufferedReader<R> {
    inner: R,
    buffer: Vec<u8>,
}

impl<R> BufferedReader<R> {
    fn new(reader: R) -> Self {
        Self {
            inner: reader,
            buffer: Vec::new(),
        }
    }
}

impl<R: Read> Read for BufferedReader<R> {
    fn read(&mut self, mut buf: &mut [u8]) -> std::io::Result<usize> {
        if self.buffer.is_empty() {
            self.inner.read(buf)
        } else {
            let n = buf.write(&self.buffer)?;
            let _ = self.buffer.drain(..n);
            Ok(n)
        }
    }
}
