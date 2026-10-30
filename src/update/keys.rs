//! The minisign public keys trusted to sign `distribution/preview.json`.
//!
//! Each entry is the base64 key line of a `minisign.pub` file (its second
//! line, `RW…`). A manifest signed by any listed key is accepted; nothing else is.
//!
//! This is the wwelder/herdr fork's own release key: its secret half stays with
//! the owner, and `minisign -S -H -m distribution/preview.json -t …` signs each
//! published preview (docs/preview/README.md, "Owner fallback").
pub(super) const RELEASE_PUBLIC_KEYS: &[&str] = &[
    // wwelder/herdr release, minisign key id 1E067B988F27A0D0.
    "RWTQoCePmHsGHidrSVZ7v2wRXfDCKaTaMx1WcJQ0BQJHh15uKU/nXqEH",
];
