#[doc(no_inline)]
pub use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::{collections::HashSet, io, time::Duration};

use io_uring::{
    cqueue, squeue,
    types::{SubmitArgs, Timespec},
    IoUring,
};
pub(crate) use libc::{sockaddr_storage, socklen_t};

use crate::driver::{Entry, Operation, Poller};

pub(crate) mod op;

/// Abstraction of io-uring operations.
pub trait OpCode {
    /// Create submission entry.
    fn create_entry(&mut self) -> squeue::Entry;
}

/// Low-level driver of io-uring.
pub struct Driver {
    inner: IoUring,
    cancelled: HashSet<u64>,
}

impl Driver {
    /// Create a new io-uring driver with 1024 entries.
    pub fn new() -> io::Result<Self> {
        Self::with_entries(1024)
    }

    /// Create a new io-uring driver with specified entries.
    pub fn with_entries(entries: u32) -> io::Result<Self> {
        Ok(Self {
            inner: IoUring::new(entries)?,
            cancelled: HashSet::default(),
        })
    }

    // Auto means that it choose to wait or not automatically.
    fn submit_auto(&mut self, timeout: Option<Duration>, wait: bool) -> io::Result<()> {
        let res = if wait {
            // Last part of submission queue, wait till timeout.
            if let Some(duration) = timeout {
                let timespec = timespec(duration);
                let args = SubmitArgs::new().timespec(&timespec);
                self.inner.submitter().submit_with_args(1, &args)
            } else {
                self.inner.submit_and_wait(1)
            }
        } else {
            self.inner.submit()
        };
        match res {
            Ok(_) => Ok(()),
            Err(e) => match e.raw_os_error() {
                Some(libc::ETIME) => Err(io::Error::from_raw_os_error(libc::ETIMEDOUT)),
                Some(libc::EBUSY) | Some(libc::EAGAIN) => Ok(()),
                _ => Err(e),
            },
        }
    }

    fn flush_submissions<'a>(&mut self, ops: &mut impl Iterator<Item = Operation<'a>>) -> bool {
        let mut ended = false;

        let mut inner_squeue = self.inner.submission();

        while !inner_squeue.is_full() {
            if let Some(mut op) = ops.next() {
                let entry = op
                    .opcode_mut()
                    .create_entry()
                    .user_data(op.user_data() as _);
                unsafe { inner_squeue.push(&entry) }.expect("queue has enough space");
            } else {
                ended = true;
                break;
            }
        }

        inner_squeue.sync();

        ended
    }

    fn poll_entries(&mut self, entries: &mut impl Extend<Entry>) {
        let completed_entries = self.inner.completion().filter_map(|entry| {
            if self.cancelled.remove(&entry.user_data()) {
                None
            } else {
                Some(create_entry(entry))
            }
        });
        entries.extend(completed_entries);
    }
}

impl Poller for Driver {
    fn attach(&mut self, _fd: RawFd) -> io::Result<()> {
        Ok(())
    }

    fn cancel(&mut self, user_data: usize) {
        self.cancelled.insert(user_data as _);
    }

    unsafe fn poll<'a>(
        &mut self,
        timeout: Option<Duration>,
        ops: &mut impl Iterator<Item = Operation<'a>>,
        entries: &mut impl Extend<Entry>,
    ) -> io::Result<()> {
        // Anyway we need to submit once, no matter there are entries in squeue.
        loop {
            let ended = self.flush_submissions(ops);

            self.submit_auto(timeout, ended)?;

            self.poll_entries(entries);

            if ended {
                break;
            }
        }
        Ok(())
    }
}

impl AsRawFd for Driver {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

fn create_entry(entry: cqueue::Entry) -> Entry {
    let result = entry.result();
    let result = if result < 0 {
        Err(io::Error::from_raw_os_error(-result))
    } else {
        Ok(result as _)
    };
    Entry::new(entry.user_data() as _, result)
}

fn timespec(duration: std::time::Duration) -> Timespec {
    Timespec::new()
        .sec(duration.as_secs())
        .nsec(duration.subsec_nanos())
}
