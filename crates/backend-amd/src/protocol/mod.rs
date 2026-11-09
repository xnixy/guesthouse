//! The guest-protocol side: the private data AMD's Windows driver sends through WDDM, and what its
//! kernel driver would answer.
//!
//! Each module checks what the guest sends and builds what the guest is owed, over plain byte
//! slices, so all of it is tested without a card or a guest. The layouts are those of one release of
//! AMD's Windows driver. Where AMD's open headers (the wkmi interop header and the debugger's
//! escape header) or the WDK name a structure or field, that name is used; the rest are named for
//! what they do.
//!
//! A field the guest computes is checked and kept as it is. A field the kernel driver would invent
//! is filled in here.

pub mod adapter_info;
pub mod allocation;
mod bytes;
