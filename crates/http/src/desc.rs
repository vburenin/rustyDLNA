//! `/rootDesc.xml` (including Xbox and Samsung DCM10 variants).

use rusty_dlna_protocol::paths::{
    CONNECTIONMGR_CONTROLURL, CONNECTIONMGR_EVENTURL, CONNECTIONMGR_PATH,
    CONTENTDIRECTORY_CONTROLURL, CONTENTDIRECTORY_EVENTURL, CONTENTDIRECTORY_PATH,
    X_MS_MEDIARECEIVERREGISTRAR_CONTROLURL, X_MS_MEDIARECEIVERREGISTRAR_EVENTURL,
    X_MS_MEDIARECEIVERREGISTRAR_PATH,
};
use rusty_dlna_protocol::soap::MEDIASERVER_TYPE;

pub const DEVICE_TYPE: &str = MEDIASERVER_TYPE;

#[derive(Clone, Debug)]
pub struct RootDescOpts {
    pub friendly_name: String,
    pub uuid: String,
    pub model_number: String,
    pub manufacturer: String,
    pub model_name: String,
    pub model_description: String,
    pub serial: String,
    pub presentation_url: Option<String>,
    pub xbox: bool,
    pub samsung_dcm10: bool,
}

impl Default for RootDescOpts {
    fn default() -> Self {
        Self {
            friendly_name: "rustyDLNA".into(),
            uuid: "uuid:00000000-0000-4000-8000-000000000001".into(),
            model_number: "1".into(),
            manufacturer: "Justin Maggard".into(),
            model_name: "Windows Media Connect compatible (rustyDLNA)".into(),
            model_description: "rustyDLNA on Linux".into(),
            serial: "1".into(),
            presentation_url: None,
            xbox: false,
            samsung_dcm10: false,
        }
    }
}

pub fn gen_root_desc(opts: &RootDescOpts) -> String {
    let mut friendly = opts.friendly_name.clone();
    let mut model_number = opts.model_number.clone();
    if opts.xbox {
        model_number = "1".into();
        if !friendly.contains(':') {
            friendly.push_str(": 1");
        }
    }
    let (mfr_url, model_url, extra) = if opts.samsung_dcm10 {
        (
            "",
            "",
            concat!(
                "<sec:ProductCap>smi,DCM10,getMediaInfo.sec,getCaptionInfo.sec</sec:ProductCap>",
                "<sec:X_ProductCap>smi,DCM10,getMediaInfo.sec,getCaptionInfo.sec</sec:X_ProductCap>"
            ),
        )
    } else {
        (
            "<manufacturerURL>http://www.netgear.com/</manufacturerURL>",
            "<modelURL>http://www.netgear.com/</modelURL>",
            "",
        )
    };
    let presentation = opts
        .presentation_url
        .as_ref()
        .map(|u| format!("<presentationURL>{u}</presentationURL>"))
        .unwrap_or_default();
    format!(
        "<?xml version=\"1.0\"?>\r\n\
         <root xmlns=\"urn:schemas-upnp-org:device-1-0\" xmlns:dlna=\"urn:schemas-dlna-org:device-1-0\">\
         <specVersion><major>1</major><minor>0</minor></specVersion>\
         <device>\
         <deviceType>{DEVICE_TYPE}</deviceType>\
         <friendlyName>{friendly}</friendlyName>\
         <manufacturer>{man}</manufacturer>\
         {mfr_url}\
         <modelDescription>{md}</modelDescription>\
         <modelName>{mn}</modelName>\
         <modelNumber>{model_number}</modelNumber>\
         {model_url}\
         <serialNumber>{sn}</serialNumber>\
         <UDN>{udn}</UDN>\
         <dlna:X_DLNADOC>DMS-1.50</dlna:X_DLNADOC>\
         {presentation}\
         {extra}\
         <iconList>\
         <icon><mimetype>image/png</mimetype><width>48</width><height>48</height><depth>24</depth><url>/icons/sm.png</url></icon>\
         <icon><mimetype>image/jpeg</mimetype><width>48</width><height>48</height><depth>24</depth><url>/icons/sm.jpg</url></icon>\
         <icon><mimetype>image/png</mimetype><width>120</width><height>120</height><depth>24</depth><url>/icons/lrg.png</url></icon>\
         <icon><mimetype>image/jpeg</mimetype><width>120</width><height>120</height><depth>24</depth><url>/icons/lrg.jpg</url></icon>\
         </iconList>\
         <serviceList>\
         <service>\
         <serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType>\
         <serviceId>urn:upnp-org:serviceId:ContentDirectory</serviceId>\
         <controlURL>{cd_ctl}</controlURL>\
         <eventSubURL>{cd_evt}</eventSubURL>\
         <SCPDURL>{cd_scpd}</SCPDURL>\
         </service>\
         <service>\
         <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>\
         <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>\
         <controlURL>{cm_ctl}</controlURL>\
         <eventSubURL>{cm_evt}</eventSubURL>\
         <SCPDURL>{cm_scpd}</SCPDURL>\
         </service>\
         <service>\
         <serviceType>urn:microsoft.com:service:X_MS_MediaReceiverRegistrar:1</serviceType>\
         <serviceId>urn:microsoft.com:serviceId:X_MS_MediaReceiverRegistrar</serviceId>\
         <controlURL>{ms_ctl}</controlURL>\
         <eventSubURL>{ms_evt}</eventSubURL>\
         <SCPDURL>{ms_scpd}</SCPDURL>\
         </service>\
         </serviceList>\
         </device></root>\r\n",
        man = opts.manufacturer,
        md = opts.model_description,
        mn = opts.model_name,
        sn = opts.serial,
        udn = opts.uuid,
        cd_ctl = CONTENTDIRECTORY_CONTROLURL,
        cd_evt = CONTENTDIRECTORY_EVENTURL,
        cd_scpd = CONTENTDIRECTORY_PATH,
        cm_ctl = CONNECTIONMGR_CONTROLURL,
        cm_evt = CONNECTIONMGR_EVENTURL,
        cm_scpd = CONNECTIONMGR_PATH,
        ms_ctl = X_MS_MEDIARECEIVERREGISTRAR_CONTROLURL,
        ms_evt = X_MS_MEDIARECEIVERREGISTRAR_EVENTURL,
        ms_scpd = X_MS_MEDIARECEIVERREGISTRAR_PATH,
    )
}

pub fn minimal_scpd() -> &'static str {
    scpd_content_directory()
}

/// dialect `genServiceDesc` for ContentDirectory. Kodi/Platinum
/// `FindAction("Browse")` requires these actions in the SCPD.
pub fn scpd_content_directory() -> &'static str {
    concat!(
        "<?xml version=\"1.0\"?>\r\n",
        "<scpd xmlns=\"urn:schemas-upnp-org:service-1-0\">",
        "<specVersion><major>1</major><minor>0</minor></specVersion>",
        "<actionList>",
        "<action><name>GetSearchCapabilities</name><argumentList>",
        "<argument><name>SearchCaps</name><direction>out</direction>",
        "<relatedStateVariable>SearchCapabilities</relatedStateVariable></argument>",
        "</argumentList></action>",
        "<action><name>GetSortCapabilities</name><argumentList>",
        "<argument><name>SortCaps</name><direction>out</direction>",
        "<relatedStateVariable>SortCapabilities</relatedStateVariable></argument>",
        "</argumentList></action>",
        "<action><name>GetSystemUpdateID</name><argumentList>",
        "<argument><name>Id</name><direction>out</direction>",
        "<relatedStateVariable>SystemUpdateID</relatedStateVariable></argument>",
        "</argumentList></action>",
        "<action><name>Browse</name><argumentList>",
        "<argument><name>ObjectID</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_ObjectID</relatedStateVariable></argument>",
        "<argument><name>BrowseFlag</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_BrowseFlag</relatedStateVariable></argument>",
        "<argument><name>Filter</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_Filter</relatedStateVariable></argument>",
        "<argument><name>StartingIndex</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_Index</relatedStateVariable></argument>",
        "<argument><name>RequestedCount</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>",
        "<argument><name>SortCriteria</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_SortCriteria</relatedStateVariable></argument>",
        "<argument><name>Result</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_Result</relatedStateVariable></argument>",
        "<argument><name>NumberReturned</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>",
        "<argument><name>TotalMatches</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>",
        "<argument><name>UpdateID</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_UpdateID</relatedStateVariable></argument>",
        "</argumentList></action>",
        "<action><name>Search</name><argumentList>",
        "<argument><name>ContainerID</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_ObjectID</relatedStateVariable></argument>",
        "<argument><name>SearchCriteria</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_SearchCriteria</relatedStateVariable></argument>",
        "<argument><name>Filter</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_Filter</relatedStateVariable></argument>",
        "<argument><name>StartingIndex</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_Index</relatedStateVariable></argument>",
        "<argument><name>RequestedCount</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>",
        "<argument><name>SortCriteria</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_SortCriteria</relatedStateVariable></argument>",
        "<argument><name>Result</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_Result</relatedStateVariable></argument>",
        "<argument><name>NumberReturned</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>",
        "<argument><name>TotalMatches</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>",
        "<argument><name>UpdateID</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_UpdateID</relatedStateVariable></argument>",
        "</argumentList></action>",
        "<action><name>UpdateObject</name><argumentList>",
        "<argument><name>ObjectID</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_ObjectID</relatedStateVariable></argument>",
        "<argument><name>CurrentTagValue</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_TagValueList</relatedStateVariable></argument>",
        "<argument><name>NewTagValue</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_TagValueList</relatedStateVariable></argument>",
        "</argumentList></action>",
        "<action><name>X_GetFeatureList</name><argumentList>",
        "<argument><name>FeatureList</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_Result</relatedStateVariable></argument>",
        "</argumentList></action>",
        "<action><name>X_SetBookmark</name><argumentList>",
        "<argument><name>ObjectID</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_ObjectID</relatedStateVariable></argument>",
        "<argument><name>PosSecond</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_PosSecond</relatedStateVariable></argument>",
        "</argumentList></action>",
        "</actionList>",
        "<serviceStateTable>",
        "<stateVariable sendEvents=\"no\"><name>SearchCapabilities</name><dataType>string</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>SortCapabilities</name><dataType>string</dataType></stateVariable>",
        "<stateVariable sendEvents=\"yes\"><name>SystemUpdateID</name><dataType>ui4</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_ObjectID</name><dataType>string</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_Result</name><dataType>string</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_SearchCriteria</name><dataType>string</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_BrowseFlag</name><dataType>string</dataType>",
        "<allowedValueList><allowedValue>BrowseMetadata</allowedValue>",
        "<allowedValue>BrowseDirectChildren</allowedValue></allowedValueList></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_Filter</name><dataType>string</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_SortCriteria</name><dataType>string</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_Index</name><dataType>ui4</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_Count</name><dataType>ui4</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_UpdateID</name><dataType>ui4</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_TagValueList</name><dataType>string</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_PosSecond</name><dataType>i4</dataType></stateVariable>",
        "</serviceStateTable></scpd>\r\n"
    )
}

pub fn scpd_connection_manager() -> &'static str {
    concat!(
        "<?xml version=\"1.0\"?>\r\n",
        "<scpd xmlns=\"urn:schemas-upnp-org:service-1-0\">",
        "<specVersion><major>1</major><minor>0</minor></specVersion>",
        "<actionList>",
        "<action><name>GetProtocolInfo</name><argumentList>",
        "<argument><name>Source</name><direction>out</direction>",
        "<relatedStateVariable>SourceProtocolInfo</relatedStateVariable></argument>",
        "<argument><name>Sink</name><direction>out</direction>",
        "<relatedStateVariable>SinkProtocolInfo</relatedStateVariable></argument>",
        "</argumentList></action>",
        "<action><name>GetCurrentConnectionIDs</name><argumentList>",
        "<argument><name>ConnectionIDs</name><direction>out</direction>",
        "<relatedStateVariable>CurrentConnectionIDs</relatedStateVariable></argument>",
        "</argumentList></action>",
        "<action><name>GetCurrentConnectionInfo</name><argumentList>",
        "<argument><name>ConnectionID</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_ConnectionID</relatedStateVariable></argument>",
        "<argument><name>RcsID</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_RcsID</relatedStateVariable></argument>",
        "<argument><name>AVTransportID</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_AVTransportID</relatedStateVariable></argument>",
        "<argument><name>ProtocolInfo</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_ProtocolInfo</relatedStateVariable></argument>",
        "<argument><name>PeerConnectionManager</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_ConnectionManager</relatedStateVariable></argument>",
        "<argument><name>PeerConnectionID</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_ConnectionID</relatedStateVariable></argument>",
        "<argument><name>Direction</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_Direction</relatedStateVariable></argument>",
        "<argument><name>Status</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_ConnectionStatus</relatedStateVariable></argument>",
        "</argumentList></action>",
        "</actionList>",
        "<serviceStateTable>",
        "<stateVariable sendEvents=\"yes\"><name>SourceProtocolInfo</name><dataType>string</dataType></stateVariable>",
        "<stateVariable sendEvents=\"yes\"><name>SinkProtocolInfo</name><dataType>string</dataType></stateVariable>",
        "<stateVariable sendEvents=\"yes\"><name>CurrentConnectionIDs</name><dataType>string</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_ConnectionStatus</name><dataType>string</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_ConnectionManager</name><dataType>string</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_Direction</name><dataType>string</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_ProtocolInfo</name><dataType>string</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_ConnectionID</name><dataType>i4</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_AVTransportID</name><dataType>i4</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_RcsID</name><dataType>i4</dataType></stateVariable>",
        "</serviceStateTable></scpd>\r\n"
    )
}

pub fn scpd_registrar() -> &'static str {
    concat!(
        "<?xml version=\"1.0\"?>\r\n",
        "<scpd xmlns=\"urn:schemas-upnp-org:service-1-0\">",
        "<specVersion><major>1</major><minor>0</minor></specVersion>",
        "<actionList>",
        "<action><name>IsAuthorized</name><argumentList>",
        "<argument><name>DeviceID</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_DeviceID</relatedStateVariable></argument>",
        "<argument><name>Result</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_Result</relatedStateVariable></argument>",
        "</argumentList></action>",
        "<action><name>IsValidated</name><argumentList>",
        "<argument><name>DeviceID</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_DeviceID</relatedStateVariable></argument>",
        "<argument><name>Result</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_Result</relatedStateVariable></argument>",
        "</argumentList></action>",
        "<action><name>RegisterDevice</name><argumentList>",
        "<argument><name>RegistrationReqMsg</name><direction>in</direction>",
        "<relatedStateVariable>A_ARG_TYPE_RegistrationReqMsg</relatedStateVariable></argument>",
        "<argument><name>RegistrationRespMsg</name><direction>out</direction>",
        "<relatedStateVariable>A_ARG_TYPE_RegistrationRespMsg</relatedStateVariable></argument>",
        "</argumentList></action>",
        "</actionList>",
        "<serviceStateTable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_DeviceID</name><dataType>string</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_RegistrationReqMsg</name><dataType>bin.base64</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_RegistrationRespMsg</name><dataType>bin.base64</dataType></stateVariable>",
        "<stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_Result</name><dataType>int</dataType></stateVariable>",
        "</serviceStateTable></scpd>\r\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mediaserver_and_three_services() {
        let xml = gen_root_desc(&RootDescOpts::default());
        assert!(xml.contains(DEVICE_TYPE));
        assert!(xml.contains("MediaServer:1"));
        assert!(xml.contains(CONTENTDIRECTORY_CONTROLURL));
        assert!(xml.contains(CONNECTIONMGR_CONTROLURL));
        assert!(xml.contains(X_MS_MEDIARECEIVERREGISTRAR_CONTROLURL));
        assert!(xml.contains("/icons/sm.png"));
    }

    #[test]
    fn xbox_lie() {
        let mut o = RootDescOpts::default();
        o.friendly_name = "rustyDLNA".into();
        o.xbox = true;
        let xml = gen_root_desc(&o);
        assert!(xml.contains("<modelNumber>1</modelNumber>"));
        assert!(xml.contains("<friendlyName>rustyDLNA: 1</friendlyName>"));
    }

    #[test]
    fn samsung_caps() {
        let mut o = RootDescOpts::default();
        o.samsung_dcm10 = true;
        let xml = gen_root_desc(&o);
        assert!(xml.contains("<sec:ProductCap>"));
        assert!(xml.contains("<sec:X_ProductCap>"));
        assert!(xml.contains("DCM10"));
    }

    #[test]
    fn contentdir_scpd_has_browse() {
        let xml = scpd_content_directory();
        assert!(xml.contains("<name>Browse</name>"));
        assert!(xml.contains("<name>Search</name>"));
        assert!(xml.contains("BrowseDirectChildren"));
        assert!(xml.contains("SystemUpdateID"));
    }
}
