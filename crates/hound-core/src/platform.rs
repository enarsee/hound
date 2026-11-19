//! The operating-system family hound runs on, passed explicitly to the decisions that
//! differ between them so both can be tested anywhere.

/// The operating-system family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Unix,
    Windows,
}

impl Platform {
    /// The platform this binary was built for.
    pub fn current() -> Platform {
        if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Unix
        }
    }
}
