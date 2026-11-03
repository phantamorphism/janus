//! The credential pool: directory-per-slot storage of real upstream
//! credentials, kind detection, refresh, and the default chain. The only
//! component with read access to credential material. Codes against
//! [`crate::contract::credential`]. See `docs/scope` "Carries over" and
//! `docs/admin-surface` "Credentials".
