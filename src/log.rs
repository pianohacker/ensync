//-
// Copyright (c) 2016, 2017, Jason Lingle
//
// This file is part of Ensync.
//
// Ensync is free software: you can  redistribute it and/or modify it under the
// terms of  the GNU General Public  License as published by  the Free Software
// Foundation, either version  3 of the License, or (at  your option) any later
// version.
//
// Ensync is distributed  in the hope that  it will be useful,  but WITHOUT ANY
// WARRANTY; without  even the implied  warranty of MERCHANTABILITY  or FITNESS
// FOR  A PARTICULAR  PURPOSE.  See the  GNU General  Public  License for  more
// details.
//
// You should have received a copy of the GNU General Public License along with
// Ensync. If not, see <http://www.gnu.org/licenses/>.

//! Defines structured logs emitted primarily by the reconciler.
//!
//! The intent is to immediately support normal "verbose" operation while also
//! providing useful output for tests, while also allowing eventual rsync-style
//! itemised output.

use std::ffi::OsStr;
use reconcile::compute::{Reconciliation,Conflict};

use defs::*;
use errors::Error;

pub type LogLevel = u8;
/// Log level indicating an unrecoverable, non-localised error.
pub const FATAL: LogLevel = 1;
/// Log level indicating a localised error.
pub const ERROR: LogLevel = 2;
/// Log level indicating a somewhat surprising situation that can still be
/// handled reasonably, such as edit/edit conflicts.
pub const WARN: LogLevel = 3;
/// Log level indicating the reconciler making changes to one of the real
/// replicas.
pub const EDIT: LogLevel = 4;
/// Log level for informational messages not indicative of problems or changes
/// being made.
pub const INFO: LogLevel = 5;

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum ReplicaSide {
    Client, Ancestor, Server
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum ErrorOperation<'a> {
    List,
    MarkClean,
    Chdir(&'a OsStr),
    Create(&'a OsStr),
    Update(&'a OsStr),
    Rename(&'a OsStr),
    Remove(&'a OsStr),
    Rmdir,
    Access(&'a OsStr),
}

// TODO We need to either actually emit `EnterDirectory` and `LeaveDirectory`
// or remove them.
#[allow(dead_code)]
#[derive(Clone,Copy,Debug)]
pub enum Log<'a> {
    EnterDirectory(&'a OsStr),
    LeaveDirectory(&'a OsStr),
    Inspect(&'a OsStr, &'a OsStr, Reconciliation, Conflict),
    Create(ReplicaSide, &'a OsStr, &'a OsStr, &'a FileData),
    Update(ReplicaSide, &'a OsStr, &'a OsStr, &'a FileData, &'a FileData),
    Rename(ReplicaSide, &'a OsStr, &'a OsStr, &'a OsStr),
    Remove(ReplicaSide, &'a OsStr, &'a OsStr, &'a FileData),
    Rmdir(ReplicaSide, &'a OsStr),
    Error(ReplicaSide, &'a OsStr, ErrorOperation<'a>, &'a Error),
}

pub trait Logger {
    fn log(&self, level: LogLevel, what: &Log);
}

#[cfg(test)]
mod println_logger {
    use std::io::{Result,Write};
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Trivial implementation of `Logger` which simply dumps everything (in
    /// debug format) to the given writer.
    pub struct PrintlnLogger<W>(Arc<Mutex<W>>);

    impl<W> PrintlnLogger<W> {
        pub fn new(w: W) -> Self {
            PrintlnLogger(Arc::new(Mutex::new(w)))
        }
    }

    impl<W> Clone for PrintlnLogger<W> {
        fn clone(&self) -> Self {
            PrintlnLogger(self.0.clone())
        }
    }

    /// Writer which logs to the same "stdout" `print!` does, since for some
    /// reason that's not accessible via any sane mechanism.
    ///
    /// Panics if a buffer written is not valid UTF-8.
    pub struct PrintWriter;
    impl Write for PrintWriter {
        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            use std::str;
            print!("{}", str::from_utf8(buf).unwrap());
            Ok(buf.len())
        }

        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }

    impl PrintlnLogger<PrintWriter> {
        pub fn stdout() -> Self {
            PrintlnLogger::new(PrintWriter)
        }
    }

    impl<W : Write> Logger for PrintlnLogger<W> {
        fn log(&self, level: LogLevel, what: &Log) {
            let level_str = match level {
                FATAL => "FATAL",
                ERROR => "ERROR",
                WARN  => " WARN",
                EDIT  => " EDIT",
                INFO  => " INFO",
                _     => "?????",
            };
            writeln!(self.0.lock().unwrap(), "[{}] {:?}",
                     level_str, what).unwrap();
        }
    }
}

#[cfg(test)]
pub use self::println_logger::{PrintlnLogger,PrintWriter};
