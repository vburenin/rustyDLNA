//! SOAP method names from `src/upnpsoap.c` `soapMethods[]`.
//! Match is a prefix (`strncmp`); first hit wins.

pub const SOAP_METHODS: &[&str] = &[
    "QueryStateVariable",
    "Browse",
    "Search",
    "GetSearchCapabilities",
    "GetSortCapabilities",
    "GetSystemUpdateID",
    "GetProtocolInfo",
    "GetCurrentConnectionIDs",
    "GetCurrentConnectionInfo",
    "IsAuthorized",
    "IsValidated",
    "RegisterDevice",
    "UpdateObject",
    "X_GetFeatureList",
    "X_SetBookmark",
];

/// Pull the method out of `SOAPAction: "urn:…:1#Browse"`.
pub fn soap_action_method(action: &str) -> Option<&'static str> {
    let rest = action.split_once('#')?.1;
    let rest = rest.trim_matches(|c| c == '"' || c == '\'');
    SOAP_METHODS
        .iter()
        .copied()
        .find(|name| rest.starts_with(name))
}

pub const DIDL_SCHEMAS: &str = concat!(
    " xmlns:dc=\"http://purl.org/dc/elements/1.1/\"",
    " xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\"",
    " xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\"",
    " xmlns:dlna=\"urn:schemas-dlna-org:metadata-1-0/\""
);
pub const DLNA_NAMESPACE: &str = " xmlns:dlna=\"urn:schemas-dlna-org:metadata-1-0/\"";
pub const PV_NAMESPACE: &str = " xmlns:pv=\"http://www.pv.com/pvns/\"";
pub const SEC_NAMESPACE: &str = " xmlns:sec=\"http://www.sec.co.kr/dlna\"";

pub const CONTENTDIRECTORY_TYPE: &str = "urn:schemas-upnp-org:service:ContentDirectory:1";
pub const CONNECTIONMANAGER_TYPE: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";
pub const MS_REGISTRAR_TYPE: &str = "urn:microsoft.com:service:X_MS_MediaReceiverRegistrar:1";
pub const MEDIASERVER_TYPE: &str = "urn:schemas-upnp-org:device:MediaServer:1";

pub const SORT_CAPS: &str =
    "dc:title,dc:date,upnp:class,upnp:album,upnp:episodeNumber,upnp:originalTrackNumber";
pub const SEARCH_CAPS: &str = concat!(
    "dc:creator,dc:date,dc:title,upnp:album,upnp:actor,upnp:artist,",
    "upnp:class,upnp:genre,@id,@parentID,@refID"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_browse_action() {
        assert_eq!(
            soap_action_method(r#""urn:schemas-upnp-org:service:ContentDirectory:1#Browse""#),
            Some("Browse")
        );
        assert_eq!(
            soap_action_method("urn:schemas-upnp-org:service:ContentDirectory:1#X_GetFeatureList"),
            Some("X_GetFeatureList")
        );
        assert_eq!(soap_action_method("urn:foo#Nope"), None);
    }

    #[test]
    fn prefix_match_like_minidlna() {
        // MiniDLNA uses strncmp; BrowseDirectChildren would hit Browse.
        assert_eq!(
            soap_action_method("urn:x#BrowseDirectChildren"),
            Some("Browse")
        );
    }
}
