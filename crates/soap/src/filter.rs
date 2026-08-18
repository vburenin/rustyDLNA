//! Browse/Search `Filter` → emit bits (`replica.md` §6 / §11).

use rusty_dlna_protocol::soap::{DIDL_SCHEMAS, DLNA_NAMESPACE, PV_NAMESPACE, SEC_NAMESPACE};

/// Which optional DIDL pieces a Browse/Search `Filter` asked for.
///
/// Phase 15 adds more standard-field bits (`dc:creator`, …). `dc_date` is
/// the one Phase 12 must honor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilterBits {
    pub dc_date: bool,
    /// `sec:CaptionInfoEx` + `sec:dcmInfo` + `xmlns:sec`.
    pub sec: bool,
    /// `pv:subtitle*` on the primary `<res>` + `xmlns:pv`.
    pub pv: bool,
    /// `xmlns:dlna`.
    pub dlna_ns: bool,
    /// Emit `<res>`. Empty/`*` or any `res` token.
    pub res: bool,
    pub res_size: bool,
    pub res_duration: bool,
    pub res_bitrate: bool,
    pub res_resolution: bool,
    pub res_sample: bool,
    pub res_channels: bool,
}

impl FilterBits {
    /// Empty / `*` for a non-Samsung client: all standard fields, no vendor ns.
    pub fn standard() -> Self {
        Self {
            dc_date: true,
            sec: false,
            pv: false,
            dlna_ns: false,
            res: true,
            res_size: true,
            res_duration: true,
            res_bitrate: true,
            res_resolution: true,
            res_sample: true,
            res_channels: true,
        }
    }
}

impl Default for FilterBits {
    fn default() -> Self {
        Self::standard()
    }
}

/// Parse ContentDirectory `Filter`.
///
/// - Empty / `*` → standard fields. Vendor `pv`/`sec` omitted **except**
///   Samsung, which gets `sec` by default.
/// - `dlna_ns` if the filter asked for `dlna` **or** the client is Samsung.
/// - `pv` if the filter lists `pv:subtitleFileType` or `pv:subtitleFileUri`.
/// - `sec` if the filter lists `sec:CaptionInfoEx` / `sec:dcmInfo`, or
///   Samsung default on empty/`*`.
/// - A listed filter that omits `dc:date` has `dc_date = false`.
pub fn parse_filter(filter: Option<&str>, samsung: bool) -> FilterBits {
    let raw = filter.map(str::trim).unwrap_or("");
    if raw.is_empty() || raw == "*" {
        return FilterBits {
            dc_date: true,
            sec: samsung,
            pv: false,
            dlna_ns: samsung,
            res: true,
            res_size: true,
            res_duration: true,
            res_bitrate: true,
            res_resolution: true,
            res_sample: true,
            res_channels: true,
        };
    }
    let has = |tok: &str| raw.contains(tok);
    FilterBits {
        dc_date: has("dc:date"),
        sec: has("sec:CaptionInfoEx") || has("sec:dcmInfo"),
        pv: has("pv:subtitleFileType") || has("pv:subtitleFileUri"),
        dlna_ns: has("dlna") || samsung,
        res: has("res"),
        res_size: has("res@size"),
        res_duration: has("res@duration"),
        res_bitrate: has("res@bitrate"),
        res_resolution: has("res@resolution"),
        res_sample: has("res@sampleFrequency"),
        res_channels: has("res@nrAudioChannels"),
    }
}

/// `DIDL-Lite` opening-tag attributes for these bits.
pub fn didl_xmlns(bits: &FilterBits) -> String {
    let mut s = String::from(DIDL_SCHEMAS);
    if bits.dlna_ns {
        s.push_str(DLNA_NAMESPACE);
    }
    if bits.pv {
        s.push_str(PV_NAMESPACE);
    }
    if bits.sec {
        s.push_str(SEC_NAMESPACE);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_star_are_standard() {
        for f in [None, Some(""), Some("  "), Some("*"), Some(" * ")] {
            let kodi = parse_filter(f, false);
            assert_eq!(kodi, FilterBits::standard(), "{f:?}");
            assert!(!kodi.sec && !kodi.pv && !kodi.dlna_ns);
            assert!(kodi.dc_date);

            let tv = parse_filter(f, true);
            assert!(tv.dc_date && tv.sec && tv.dlna_ns && !tv.pv, "{f:?}");
        }
    }

    #[test]
    fn listed_fields_drop_omitted_dc_date() {
        let bits = parse_filter(Some("dc:title,upnp:class,res"), false);
        assert!(!bits.dc_date);
        assert!(!bits.sec && !bits.pv && !bits.dlna_ns);

        let with_date = parse_filter(Some("dc:title,dc:date,res"), false);
        assert!(with_date.dc_date);
    }

    #[test]
    fn vendor_tokens_set_bits() {
        let sec = parse_filter(Some("dc:title,sec:CaptionInfoEx"), false);
        assert!(sec.sec && !sec.pv && !sec.dlna_ns && !sec.dc_date);

        let dcm = parse_filter(Some("sec:dcmInfo"), false);
        assert!(dcm.sec);

        let pv = parse_filter(Some("pv:subtitleFileType,pv:subtitleFileUri"), false);
        assert!(pv.pv && !pv.sec);

        let one_pv = parse_filter(Some("res,pv:subtitleFileUri"), false);
        assert!(one_pv.pv);

        let dlna = parse_filter(Some("dlna:profileID"), false);
        assert!(dlna.dlna_ns);
    }

    #[test]
    fn samsung_listed_filter_still_gets_dlna_ns() {
        let bits = parse_filter(Some("dc:title,dc:date"), true);
        assert!(bits.dc_date && bits.dlna_ns);
        assert!(!bits.sec, "Samsung sec default is empty/* only");
        assert!(!bits.pv);
    }

    #[test]
    fn xmlns_order_is_dlna_then_pv_then_sec() {
        let listed = parse_filter(Some("dc:title,upnp:class"), false);
        assert!(!listed.res && !listed.res_size && !listed.dc_date);
        let res_only = parse_filter(Some("res"), false);
        assert!(res_only.res && !res_only.res_size);
        let sized = parse_filter(Some("res,res@size"), false);
        assert!(sized.res && sized.res_size);

        let xmlns = didl_xmlns(&FilterBits {
            dc_date: true,
            sec: true,
            pv: true,
            dlna_ns: true,
            ..FilterBits::standard()
        });
        let dlna = xmlns.find("xmlns:dlna=").unwrap();
        let pv = xmlns.find("xmlns:pv=").unwrap();
        let sec = xmlns.find("xmlns:sec=").unwrap();
        assert!(dlna < pv && pv < sec, "{xmlns}");
        assert!(xmlns.contains("http://purl.org/dc/elements/1.1/"));
    }
}
