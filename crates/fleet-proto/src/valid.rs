//! Validated values. Each type checks its invariant in its constructor (`TryFrom`, `FromStr`)
//! and its deserializer goes through that constructor, so no frame can carry an invalid one.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Why a value was rejected. The message names the field, never echoes the value, so it is
/// safe to return to an unauthenticated peer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Invalid {
    #[error("{field} is empty")]
    Empty { field: &'static str },
    #[error("{field} is longer than {max} bytes")]
    TooLong { field: &'static str, max: usize },
    #[error("{field} contains a character it may not")]
    BadChar { field: &'static str },
    #[error("{field} must be an absolute path")]
    NotAbsolute { field: &'static str },
    #[error("{field} must be between {min} and {max}")]
    OutOfRange {
        field: &'static str,
        min: u64,
        max: u64,
    },
    #[error("{field} has more than {max} entries")]
    TooMany { field: &'static str, max: usize },
}

/// Checks a name: 1..=`max` bytes, the first an ASCII alphanumeric (so it can never read as
/// an option), the rest alphanumerics or one of `extra`.
fn check_name(field: &'static str, s: &str, max: usize, extra: &[u8]) -> Result<(), Invalid> {
    let bytes = s.as_bytes();
    let Some(first) = bytes.first() else {
        return Err(Invalid::Empty { field });
    };
    if bytes.len() > max {
        return Err(Invalid::TooLong { field, max });
    }
    let ok = first.is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || extra.contains(b));
    if ok {
        Ok(())
    } else {
        Err(Invalid::BadChar { field })
    }
}

macro_rules! string_newtype {
    ($(#[$doc:meta])* $name:ident, $check:expr) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String")]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = Invalid;

            fn try_from(s: String) -> Result<Self, Invalid> {
                let check: fn(&str) -> Result<(), Invalid> = $check;
                check(&s)?;
                Ok(Self(s))
            }
        }

        impl FromStr for $name {
            type Err = Invalid;

            fn from_str(s: &str) -> Result<Self, Invalid> {
                Self::try_from(s.to_owned())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

string_newtype!(
    /// An adapter's name: `[a-z0-9][a-z0-9_-]{0,31}`.
    AdapterName,
    |s| {
        check_name("adapter", s, 32, b"_-")?;
        if s.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(Invalid::BadChar { field: "adapter" });
        }
        Ok(())
    }
);

string_newtype!(
    /// A device name: `[a-z0-9][a-z0-9.-]{0,62}`, usable as a DNS SAN. fleetd's `device` key
    /// and the hub's registry keys both follow it.
    DeviceName,
    |s| {
        check_name("device", s, 63, b".-")?;
        if s.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(Invalid::BadChar { field: "device" });
        }
        Ok(())
    }
);

string_newtype!(
    /// A model name: `[A-Za-z0-9][A-Za-z0-9._:/@+-]{0,127}`. A daemon also requires it to
    /// be in the adapter's `models` allowlist.
    ModelName,
    |s| check_name("model", s, 128, b"._:/@+-")
);

string_newtype!(
    /// A session id: `[A-Za-z0-9][A-Za-z0-9_-]{0,63}`. fleetd mints them: `d-<unix start>-<8 hex>`
    /// for a session it dispatched, `p-<pid>-<unix start>` for one it discovered.
    SessionId,
    |s| check_name("session", s, 64, b"_-")
);

string_newtype!(
    /// A label key: `[a-z0-9][a-z0-9_.-]{0,63}`.
    LabelKey,
    |s| {
        check_name("label key", s, 64, b"_.-")?;
        if s.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(Invalid::BadChar { field: "label key" });
        }
        Ok(())
    }
);

string_newtype!(
    /// A working directory as the requester names it: absolute, at most 4096 bytes, no NUL.
    /// It is not trusted as a location: the daemon canonicalizes it and checks it against
    /// the adapter's `workdir_roots`.
    Workdir,
    |s| {
        const FIELD: &str = "workdir";
        if s.is_empty() {
            return Err(Invalid::Empty { field: FIELD });
        }
        if s.len() > Workdir::MAX_LEN {
            return Err(Invalid::TooLong { field: FIELD, max: Workdir::MAX_LEN });
        }
        if s.contains('\0') {
            return Err(Invalid::BadChar { field: FIELD });
        }
        if !s.starts_with('/') {
            return Err(Invalid::NotAbsolute { field: FIELD });
        }
        Ok(())
    }
);

impl Workdir {
    pub const MAX_LEN: usize = 4096;

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
}

string_newtype!(
    /// The prompt written to the agent's stdin: 1..=[`Prompt::MAX_LEN`] bytes of UTF-8 with
    /// no control characters (C0, DEL, C1) other than tab, line feed and carriage return.
    Prompt,
    |s| {
        const FIELD: &str = "prompt";
        if s.is_empty() {
            return Err(Invalid::Empty { field: FIELD });
        }
        if s.len() > Prompt::MAX_LEN {
            return Err(Invalid::TooLong { field: FIELD, max: Prompt::MAX_LEN });
        }
        if s.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')) {
            return Err(Invalid::BadChar { field: FIELD });
        }
        Ok(())
    }
);

impl Prompt {
    /// 256 KiB. With control characters excluded, JSON escaping at most doubles a prompt
    /// (`serde_json` escapes only quotes, backslashes and C0 controls),
    /// so a dispatch request always fits in one frame.
    pub const MAX_LEN: usize = 256 * 1024;
}

/// Free-form labels a dispatcher attaches to a session, such as `issue = "42"`. At most
/// [`Labels::MAX_ENTRIES`] entries; values are at most [`Labels::MAX_VALUE_LEN`] bytes with
/// no control characters, because dashboards display them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "BTreeMap<String, String>")]
pub struct Labels(BTreeMap<LabelKey, String>);

impl Labels {
    pub const MAX_ENTRIES: usize = 16;
    pub const MAX_VALUE_LEN: usize = 256;

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&LabelKey, &str)> {
        self.0.iter().map(|(k, v)| (k, v.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl TryFrom<BTreeMap<String, String>> for Labels {
    type Error = Invalid;

    fn try_from(map: BTreeMap<String, String>) -> Result<Self, Invalid> {
        if map.len() > Self::MAX_ENTRIES {
            return Err(Invalid::TooMany {
                field: "labels",
                max: Self::MAX_ENTRIES,
            });
        }
        let mut out = BTreeMap::new();
        for (k, v) in map {
            if v.len() > Self::MAX_VALUE_LEN {
                return Err(Invalid::TooLong {
                    field: "label value",
                    max: Self::MAX_VALUE_LEN,
                });
            }
            if v.chars().any(char::is_control) {
                return Err(Invalid::BadChar {
                    field: "label value",
                });
            }
            out.insert(LabelKey::try_from(k)?, v);
        }
        Ok(Self(out))
    }
}

macro_rules! bounded_u32 {
    ($(#[$doc:meta])* $name:ident, $field:literal, $min:expr, $max:expr) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "u64")]
        pub struct $name(u32);

        impl $name {
            pub const MIN: u32 = $min;
            pub const MAX: u32 = $max;

            pub fn get(self) -> u32 {
                self.0
            }
        }

        impl TryFrom<u64> for $name {
            type Error = Invalid;

            fn try_from(n: u64) -> Result<Self, Invalid> {
                u32::try_from(n)
                    .ok()
                    .filter(|v| (Self::MIN..=Self::MAX).contains(v))
                    .map(Self)
                    .ok_or(Invalid::OutOfRange {
                        field: $field,
                        min: u64::from(Self::MIN),
                        max: u64::from(Self::MAX),
                    })
            }
        }

        impl FromStr for $name {
            type Err = Invalid;

            fn from_str(s: &str) -> Result<Self, Invalid> {
                let n: u64 = s.parse().map_err(|_| Invalid::OutOfRange {
                    field: $field,
                    min: u64::from(Self::MIN),
                    max: u64::from(Self::MAX),
                })?;
                Self::try_from(n)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

bounded_u32!(
    /// A dispatch timeout in seconds: 1 to 604800 (seven days). The daemon uses the smaller
    /// of this and the adapter's `timeout_s`.
    TimeoutSecs,
    "timeout_s",
    1,
    604_800
);

bounded_u32!(
    /// How many trailing log lines a `logs` request returns: 1 to 10000, default 200.
    LogLines,
    "lines",
    1,
    10_000
);

impl Default for LogLines {
    fn default() -> Self {
        Self(200)
    }
}
