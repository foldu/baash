//! Errors raised by shell state, and the POSIX §2.8.1 consequence table.
//!
//! Two error types live here on purpose:
//!
//! * [`Error`] is a *state operation* failure: a readonly write, an unknown
//!   option, a shift underflow. It is returned by [`crate::Vars`] and
//!   [`crate::Opts`] so callers cannot accidentally ignore it.
//! * [`ShellError`] is a *command execution* error class from POSIX §2.8.1.
//!   Whether it makes the shell exit depends on interactivity (and, for
//!   special built-ins, on whether the utility was reached through
//!   `command`); [`consequence`] is that table, and nothing else should encode
//!   it.

use crate::{OptionKind, Status};
use bsh_diag::{Diagnostic, ErrCode};

/// Stable diagnostic codes for shell state errors.
pub mod codes {
    use bsh_diag::ErrCode;

    /// Assignment or attribute change on a readonly variable.
    pub const READONLY: ErrCode = ErrCode::new(500);
    /// `set -o`/`shopt` named an option that does not exist.
    pub const UNKNOWN_OPTION: ErrCode = ErrCode::new(501);
    /// A valid option used from the wrong table (`set -o extglob`).
    pub const WRONG_OPTION_KIND: ErrCode = ErrCode::new(502);
    /// A variable name is not `[A-Za-z_][A-Za-z0-9_]*`.
    pub const INVALID_NAME: ErrCode = ErrCode::new(503);
    /// `pop_frame` on an empty scope stack.
    pub const FRAME_UNDERFLOW: ErrCode = ErrCode::new(504);
    /// `shift n` with `n` greater than `$#`.
    pub const SHIFT_UNDERFLOW: ErrCode = ErrCode::new(505);
    /// An array operation does not match the variable's shape.
    pub const ARRAY_SHAPE: ErrCode = ErrCode::new(506);
    /// A nameref chain loops or exceeds the depth limit.
    pub const NAMEREF_CYCLE: ErrCode = ErrCode::new(507);
    /// A builtin or runtime feature is present in the table but not written.
    pub const NOT_IMPLEMENTED: ErrCode = ErrCode::new(508);
    /// A builtin operand was not a valid decimal count.
    pub const BAD_NUMBER: ErrCode = ErrCode::new(509);
    /// A builtin was called with an unknown option or wrong operand count.
    pub const USAGE: ErrCode = ErrCode::new(510);
}

/// A failed shell-state operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A readonly variable was written or unset.
    Readonly {
        /// Variable name.
        name: Vec<u8>,
    },
    /// No option with this name exists.
    UnknownOption {
        /// The name as written, without `-o`.
        name: Vec<u8>,
    },
    /// The option exists but not in the requested table.
    WrongOptionKind {
        /// Option name.
        name: Vec<u8>,
        /// Table the caller required.
        kind: OptionKind,
    },
    /// The name is not a valid shell variable name.
    InvalidName {
        /// The offending name.
        name: Vec<u8>,
    },
    /// `pop_frame` was called with no frame on the stack.
    FrameUnderflow,
    /// `shift n` with `n > $#`.
    ShiftUnderflow {
        /// Requested shift count.
        requested: usize,
        /// Number of positional parameters available.
        available: usize,
    },
    /// A bare assignment was attempted on an associative array.
    AssocNeedsSubscript {
        /// Variable name.
        name: Vec<u8>,
    },
    /// An indexed subscript was used on an associative array.
    NotIndexed {
        /// Variable name.
        name: Vec<u8>,
    },
    /// An associative subscript was used on an indexed array.
    NotAssociative {
        /// Variable name.
        name: Vec<u8>,
    },
    /// The requested array shape conflicts with the variable's current shape.
    ArrayShapeMismatch {
        /// Variable name.
        name: Vec<u8>,
    },
    /// A nameref chain loops or is longer than the supported limit.
    NamerefCycle {
        /// Variable name the lookup started from.
        name: Vec<u8>,
    },
    /// A builtin operand was not a decimal number.
    BadNumber {
        /// The operand as written.
        value: Vec<u8>,
    },
    /// A scalar operation was attempted on an array.
    NotScalar {
        /// Variable name.
        name: Vec<u8>,
    },
    /// A negative subscript resolved before the start of the array.
    ArrayOutOfBounds {
        /// Variable name.
        name: Vec<u8>,
        /// The subscript as written.
        index: i64,
    },
}

impl Error {
    /// Stable diagnostic code for this error (see [`codes`]).
    pub fn code(&self) -> ErrCode {
        match self {
            Error::Readonly { .. } => codes::READONLY,
            Error::UnknownOption { .. } => codes::UNKNOWN_OPTION,
            Error::WrongOptionKind { .. } => codes::WRONG_OPTION_KIND,
            Error::InvalidName { .. } => codes::INVALID_NAME,
            Error::FrameUnderflow => codes::FRAME_UNDERFLOW,
            Error::ShiftUnderflow { .. } => codes::SHIFT_UNDERFLOW,
            Error::AssocNeedsSubscript { .. }
            | Error::NotIndexed { .. }
            | Error::NotAssociative { .. }
            | Error::ArrayShapeMismatch { .. } => codes::ARRAY_SHAPE,
            Error::NamerefCycle { .. } => codes::NAMEREF_CYCLE,
            Error::BadNumber { .. } => codes::BAD_NUMBER,
            Error::NotScalar { .. } | Error::ArrayOutOfBounds { .. } => codes::ARRAY_SHAPE,
        }
    }

    /// Human-readable message. Names are decoded lossily *for display only*;
    /// the bytes are kept untouched in the error.
    pub fn message(&self) -> String {
        match self {
            Error::Readonly { name } => {
                format!("{}: readonly variable", display(name))
            }
            Error::UnknownOption { name } => {
                format!("{}: unknown option", display(name))
            }
            Error::WrongOptionKind { name, kind } => {
                format!("{}: not a {} option", display(name), kind.as_str())
            }
            Error::InvalidName { name } => {
                format!("{}: not a valid identifier", display(name))
            }
            Error::FrameUnderflow => "scope stack underflow".to_owned(),
            Error::ShiftUnderflow {
                requested,
                available,
            } => format!("shift: {requested}: shift count out of range (there are {available})"),
            Error::AssocNeedsSubscript { name } => {
                format!(
                    "{}: must use subscript when assigning associative array",
                    display(name)
                )
            }
            Error::NotIndexed { name } => {
                format!("{}: not an indexed array", display(name))
            }
            Error::NotAssociative { name } => {
                format!("{}: not an associative array", display(name))
            }
            Error::ArrayShapeMismatch { name } => {
                format!("{}: array shape mismatch", display(name))
            }
            Error::NamerefCycle { name } => {
                format!("{}: circular name reference", display(name))
            }
            Error::BadNumber { value } => {
                format!("{}: numeric argument required", display(value))
            }
            Error::NotScalar { name } => {
                format!("{}: not a scalar", display(name))
            }
            Error::ArrayOutOfBounds { name, index } => {
                format!("{}[{index}]: index out of bounds", display(name))
            }
        }
    }

    /// Convert to a diagnostic. State errors have no source span, so the span
    /// is `0..0`; the VM can rebuild one from the chunk span table.
    pub fn diagnostic(&self) -> Diagnostic {
        Diagnostic::error(self.code(), 0..0, self.message())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for Error {}

/// Render arbitrary shell bytes for a human, replacing invalid UTF-8 rather
/// than losing offsets or panicking.
pub fn display(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A command-execution error class, one row of POSIX §2.8.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellError {
    /// The shell language could not be parsed.
    Syntax,
    /// A special built-in failed. `via_command` is true when it was run
    /// through the `command` utility, which suppresses the exit (note 1).
    SpecialBuiltin {
        /// The built-in's status.
        status: Status,
        /// True when reached through `command`.
        via_command: bool,
    },
    /// A redirection failed while setting up a special built-in.
    SpecialBuiltinRedirection,
    /// A utility that is not a special built-in failed.
    Utility {
        /// The utility's status.
        status: Status,
        /// Whether the failure was a redirection error.
        redirection: bool,
    },
    /// A redirection failed while setting up a compound command.
    CompoundRedirection,
    /// A redirection failed while setting up a function call.
    FunctionRedirection,
    /// A variable assignment failed (including readonly writes).
    VariableAssignment,
    /// Word expansion failed (including `nounset`).
    Expansion,
    /// The command name was not found.
    CommandNotFound,
    /// An unrecoverable error occurred while reading commands.
    ReadError,
}

/// What the shell must do after an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Write the diagnostic and keep going.
    Continue,
    /// Write the diagnostic and exit with this status.
    Exit(Status),
}

impl ShellError {
    /// Record an error arising from a builtin invocation of `tier`'s failure.
    pub fn from_builtin(tier: crate::Tier, status: Status) -> ShellError {
        match tier {
            crate::Tier::Special => ShellError::SpecialBuiltin {
                status,
                via_command: false,
            },
            _ => ShellError::Utility {
                status,
                redirection: false,
            },
        }
    }
}

/// The POSIX §2.8.1 table: is this error fatal, given interactivity?
///
/// `direct` special-built-in execution is the default; use
/// [`ShellError::SpecialBuiltin`] with `via_command: true` for `command
/// builtin`, which never exits the shell. `errexit` is deliberately *not*
/// folded in here — it is a separate mechanism (DESIGN.md §4.7) applied by the
/// VM after this table.
///
/// The standard says "shall exit" without fixing a status for most rows; the
/// statuses here are bash's (2 for syntax errors, 1 otherwise) and are
/// documented so they can be changed in one place.
pub fn consequence(error: &ShellError, interactive: bool) -> Action {
    use ShellError::{CommandNotFound, Expansion, ReadError, Syntax, VariableAssignment};

    match *error {
        Syntax => {
            if interactive {
                Action::Continue
            } else {
                Action::Exit(Status(2))
            }
        }
        ShellError::SpecialBuiltin {
            via_command: true, ..
        } => Action::Continue,
        ShellError::SpecialBuiltin { .. } => {
            if interactive {
                Action::Continue
            } else {
                // POSIX note 1: the shell "shall exit 1".
                Action::Exit(Status(1))
            }
        }
        ShellError::SpecialBuiltinRedirection => {
            if interactive {
                Action::Continue
            } else {
                Action::Exit(Status(1))
            }
        }
        ShellError::Utility { .. }
        | ShellError::CompoundRedirection
        | ShellError::FunctionRedirection
        | CommandNotFound => Action::Continue,
        VariableAssignment | Expansion => {
            if interactive {
                Action::Continue
            } else {
                Action::Exit(Status(1))
            }
        }
        ReadError => Action::Exit(Status(1)),
    }
}
