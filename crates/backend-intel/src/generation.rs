//! What one graphics architecture changes, picked by the IP version xe reports for the card.
//!
//! The adapter record names the platform and core families, which have to be right for the card,
//! so those come from here, along with what the card can't be asked for. The numbers are Intel's
//! open ones: the family and core enums from gmmlib's `igfxfmid.h`, and the rest from
//! compute-runtime's hardware tables for the platform. Both are MIT. A card whose IP has no profile
//! is refused, as the guest would be told the wrong platform.

use crate::xe_query::IpVersion;

/// One architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Generation {
    pub name: &'static str,
    /// The render IP's major and minor version, as xe gives them.
    pub render_ip: (u16, u16),
    /// PRODUCT_FAMILY.
    pub product_family: u32,
    /// PCH_PRODUCT_FAMILY. The platform has no separate PCH to name.
    pub pch_family: u32,
    /// GFXCORE_FAMILY of the display and render cores.
    pub display_core: u32,
    pub render_core: u32,
    /// PLATFORM_TYPE.
    pub platform_type: u32,
    /// GTTYPE. Newer platforms leave it undefined.
    pub gt_type: u32,
    /// Hardware threads in each EU, for a card whose hwconfig doesn't say.
    pub threads_per_eu: u32,
}

/// Lunar Lake's graphics, the Arc 130V and 140V: IGFX_LUNARLAKE on IGFX_XE2_LPG_CORE.
pub const XE2_LPG: Generation = Generation {
    name: "Xe2 LPG (Lunar Lake)",
    render_ip: (20, 4),
    product_family: 1275,
    pch_family: 0,
    display_core: 0x0C09,
    render_core: 0x0C09,
    platform_type: 0,
    gt_type: 18,
    threads_per_eu: 8,
};

const KNOWN: [Generation; 1] = [XE2_LPG];

/// The profile for a card whose render IP is `ip`, if there is one. The revision doesn't matter.
pub fn for_render_ip(ip: IpVersion) -> Option<&'static Generation> {
    KNOWN.iter().find(|generation| generation.render_ip == (ip.major, ip.minor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lunar_lake_is_found_by_its_render_ip_at_any_revision() {
        for revision in [0, 4] {
            let ip = IpVersion { major: 20, minor: 4, revision };
            assert_eq!(for_render_ip(ip), Some(&XE2_LPG));
        }
    }

    #[test]
    fn another_ip_has_no_profile() {
        assert_eq!(for_render_ip(IpVersion { major: 20, minor: 1, revision: 0 }), None);
        assert_eq!(for_render_ip(IpVersion::default()), None, "a kernel that gives no IP version");
    }
}
