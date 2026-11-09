//! A new context's private data: a fixed header, and an extension after it when a flag says so.
//!
//! The guest computes all of it and the kernel driver only checks it. Nothing is written back, so
//! the data returns to the guest as it came. The fields this check doesn't name are copied by the
//! kernel into its own context and never reach the guest again.

use super::bytes::dword;

const HEADER_SIZE: usize = 0x40;
const EXTENSION_SIZE: usize = 0x168;

/// The header's own size field, which must be [`HEADER_SIZE`].
const OWN_SIZE: usize = 0x00;
const FLAGS: usize = 0x04;
/// Two flags old drivers set and this one no longer accepts.
const FLAGS_OBSOLETE: u32 = 0b11 << 2;
/// The context asks for a VMID a debugger can attach to.
const FLAG_DEBUG_VMID: u32 = 1 << 6;
/// [`CLIENT_ID`] holds a client id.
const FLAG_HAS_CLIENT_ID: u32 = 1 << 18;
/// An extension of [`EXTENSION_SIZE`] bytes follows the header.
const FLAG_HAS_EXTENSION: u32 = 1 << 20;
const SESSION_FLAGS: usize = 0x14;
/// The context is a display encoding session, which must run on a real-time video encode
/// scheduler.
const SESSION_DISPLAY_ENCODING: u32 = 1 << 0;
const CLIENT_ID: usize = 0x34;
/// Client ids run from one to this.
const LAST_CLIENT_ID: u32 = 10;
/// The extension's own size field, which must be [`EXTENSION_SIZE`].
const EXTENSION_OWN_SIZE: usize = 0x40;

/// Why the kernel driver would refuse this private data, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused(pub &'static str);

/// What a valid header says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Context {
    pub flags: u32,
    pub client_id: Option<u32>,
    pub has_extension: bool,
}

/// Checks the private data as the kernel driver would.
///
/// Data shorter than a header carries nothing to check. The kernel takes it for the contexts the
/// guest's own kernel makes, which are the only ones that send it, so it is taken here too.
///
/// Two things are refused that the kernel would take on a real card, because nothing here backs
/// them: a debugger's VMID, as the host's VMIDs are the host's own, and a display encoding session,
/// as no video engine is offered.
pub fn check(data: &[u8]) -> Result<Option<Context>, Refused> {
    if data.len() < HEADER_SIZE {
        return Ok(None);
    }
    let field = |at: usize| dword(data, at).ok_or(Refused("the header runs past the end"));
    if field(OWN_SIZE)? != HEADER_SIZE as u32 {
        return Err(Refused("the header has the wrong size"));
    }
    let flags = field(FLAGS)?;
    if flags & FLAGS_OBSOLETE != 0 {
        return Err(Refused("an obsolete flag is set"));
    }
    if flags & FLAG_DEBUG_VMID != 0 {
        return Err(Refused("a debugger VMID is asked for"));
    }
    if field(SESSION_FLAGS)? & SESSION_DISPLAY_ENCODING != 0 {
        return Err(Refused("a display encoding session is asked for"));
    }
    let id = field(CLIENT_ID)?;
    let client_id = if flags & FLAG_HAS_CLIENT_ID != 0 {
        if !(1..=LAST_CLIENT_ID).contains(&id) {
            return Err(Refused("the client id is out of range"));
        }
        Some(id)
    } else if id != 0 {
        return Err(Refused("a client id is given with no flag for it"));
    } else {
        None
    };
    let has_extension = flags & FLAG_HAS_EXTENSION != 0;
    if has_extension {
        if data.len() < HEADER_SIZE + EXTENSION_SIZE {
            return Err(Refused("the extension is cut short"));
        }
        if field(EXTENSION_OWN_SIZE)? != EXTENSION_SIZE as u32 {
            return Err(Refused("the extension has the wrong size"));
        }
    }
    Ok(Some(Context { flags, client_id, has_extension }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::bytes::put_dword;

    fn header(flags: u32) -> Vec<u8> {
        let mut data = vec![0u8; HEADER_SIZE];
        put_dword(&mut data, OWN_SIZE, HEADER_SIZE as u32);
        put_dword(&mut data, FLAGS, flags);
        data
    }

    fn with_extension(flags: u32) -> Vec<u8> {
        let mut data = header(flags | FLAG_HAS_EXTENSION);
        data.resize(HEADER_SIZE + EXTENSION_SIZE, 0);
        put_dword(&mut data, EXTENSION_OWN_SIZE, EXTENSION_SIZE as u32);
        data
    }

    fn refused(data: &[u8]) -> &'static str {
        check(data).unwrap_err().0
    }

    #[test]
    fn a_plain_header_is_taken() {
        let context = check(&header(0)).unwrap().unwrap();
        assert_eq!(context, Context { flags: 0, client_id: None, has_extension: false });
    }

    #[test]
    fn data_too_short_for_a_header_is_taken_unread() {
        assert_eq!(check(&[]), Ok(None));
        assert_eq!(check(&[0xFF; HEADER_SIZE - 1]), Ok(None));
    }

    #[test]
    fn the_header_must_give_its_size() {
        let mut data = header(0);
        put_dword(&mut data, OWN_SIZE, 0x30);
        assert_eq!(refused(&data), "the header has the wrong size");
    }

    #[test]
    fn either_obsolete_flag_is_refused() {
        assert_eq!(refused(&header(1 << 2)), "an obsolete flag is set");
        assert_eq!(refused(&header(1 << 3)), "an obsolete flag is set");
        assert!(check(&header(1 << 1 | 1 << 4)).is_ok());
    }

    #[test]
    fn a_debugger_vmid_and_an_encoding_session_are_refused() {
        assert_eq!(refused(&header(FLAG_DEBUG_VMID)), "a debugger VMID is asked for");
        let mut data = header(0);
        put_dword(&mut data, SESSION_FLAGS, SESSION_DISPLAY_ENCODING);
        assert_eq!(refused(&data), "a display encoding session is asked for");
    }

    #[test]
    fn a_client_id_needs_its_flag_and_a_range() {
        let mut data = header(FLAG_HAS_CLIENT_ID);
        put_dword(&mut data, CLIENT_ID, 3);
        assert_eq!(check(&data).unwrap().unwrap().client_id, Some(3));
        put_dword(&mut data, CLIENT_ID, 0);
        assert_eq!(refused(&data), "the client id is out of range");
        put_dword(&mut data, CLIENT_ID, LAST_CLIENT_ID + 1);
        assert_eq!(refused(&data), "the client id is out of range");
        let mut unflagged = header(0);
        put_dword(&mut unflagged, CLIENT_ID, 3);
        assert_eq!(refused(&unflagged), "a client id is given with no flag for it");
    }

    #[test]
    fn the_extension_must_be_there_and_sized() {
        assert!(check(&with_extension(0)).unwrap().unwrap().has_extension);
        let mut short = with_extension(0);
        short.truncate(HEADER_SIZE + EXTENSION_SIZE - 1);
        assert_eq!(refused(&short), "the extension is cut short");
        let mut wrong = with_extension(0);
        put_dword(&mut wrong, EXTENSION_OWN_SIZE, 0x100);
        assert_eq!(refused(&wrong), "the extension has the wrong size");
    }
}
