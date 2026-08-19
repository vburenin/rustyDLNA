/* MiniUPnP project
 * http://miniupnp.free.fr/ or http://miniupnp.tuxfamily.org/
 *
 * Copyright (c) 2006-2008, Thomas Bernard
 * All rights reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions are met:
 *     * Redistributions of source code must retain the above copyright
 *       notice, this list of conditions and the following disclaimer.
 *     * Redistributions in binary form must reproduce the above copyright
 *       notice, this list of conditions and the following disclaimer in the
 *       documentation and/or other materials provided with the distribution.
 *     * The name of the author may not be used to endorse or promote products
 *       derived from this software without specific prior written permission.
 *
 * THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
 * AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
 * ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE
 * LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
 * CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
 * SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
 * INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
static const char * const upnpallowedvalues[] =
{
	0,			/* 0 */
	"DSL",			/* 1 */
	"POTS",
	"Cable",
	"Ethernet",
	0,
	"Up",			/* 6 */
	"Down",
	"Initializing",
	"Unavailable",
	0,
	"TCP",			/* 11 */
	"UDP",
	0,
	"Unconfigured",		/* 14 */
	"IP_Routed",
	"IP_Bridged",
	0,
	"Unconfigured",		/* 18 */
	"Connecting",
	"Connected",
	"PendingDisconnect",
	"Disconnecting",
	"Disconnected",
	0,
	"ERROR_NONE",		/* 25 */
	0,
	"OK",			/* 27 */
	"ContentFormatMismatch",
	"InsufficientBandwidth",
	"UnreliableChannel",
	"Unknown",
	0,
	"Input",		/* 33 */
	"Output",
	0,
	"BrowseMetadata",	/* 36 */
	"BrowseDirectChildren",
	0,
	"COMPLETED",		/* 39 */
	"ERROR",
	"IN_PROGRESS",
	"STOPPED",
	0,
	RESOURCE_PROTOCOL_INFO_VALUES,		/* 44 */
	0,
	"0",			/* 46 */
	0,
	"",			/* 48 */
	0
};

static const char xmlver[] = 
	"<?xml version=\"1.0\"?>\r\n";
static const char root_service[] =
	"scpd xmlns=\"urn:schemas-upnp-org:service-1-0\"";
static const char root_device[] = 
	"root xmlns=\"urn:schemas-upnp-org:device-1-0\"";

/* root Description of the UPnP Device */
static const struct XMLElt rootDesc[] =
{
	{root_device, INITHELPER(1,2)},
	{"specVersion", INITHELPER(3,2)},
	{"device", INITHELPER(5,(14))},
	{"/major", "1"},
	{"/minor", "0"},
	{"/deviceType", "urn:schemas-upnp-org:device:MediaServer:1"},
	{"/friendlyName", friendly_name},	/* required */
	{"/manufacturer", ROOTDEV_MANUFACTURER},		/* required */
	{"/manufacturerURL", ROOTDEV_MANUFACTURERURL},	/* optional */
	{"/modelDescription", ROOTDEV_MODELDESCRIPTION}, /* recommended */
	{"/modelName", modelname},	/* required */
	{"/modelNumber", modelnumber},
	{"/modelURL", ROOTDEV_MODELURL},
	{"/serialNumber", serialnumber},
	{"/UDN", uuidvalue},	/* required */
	{"/dlna:X_DLNADOC xmlns:dlna=\"urn:schemas-dlna-org:device-1-0\"", "DMS-1.50"},
	{"/presentationURL", presentationurl},	/* recommended */
	{"iconList", INITHELPER((19),4)},
	{"serviceList", INITHELPER((43),3)},
	{"icon", INITHELPER((23),5)},
	{"icon", INITHELPER((28),5)},
	{"icon", INITHELPER((33),5)},
	{"icon", INITHELPER((38),5)},
	{"/mimetype", "image/png"},
	{"/width", "48"},
	{"/height", "48"},
	{"/depth", "24"},
	{"/url", "/icons/sm.png"},
	{"/mimetype", "image/png"},
	{"/width", "120"},
	{"/height", "120"},
	{"/depth", "24"},
	{"/url", "/icons/lrg.png"},
	{"/mimetype", "image/jpeg"},
	{"/width", "48"},
	{"/height", "48"},
	{"/depth", "24"},
	{"/url", "/icons/sm.jpg"},
	{"/mimetype", "image/jpeg"},
	{"/width", "120"},
	{"/height", "120"},
	{"/depth", "24"},
	{"/url", "/icons/lrg.jpg"},
	{"service", INITHELPER((46),5)},
	{"service", INITHELPER((51),5)},
	{"service", INITHELPER((56),5)},
	{"/serviceType", "urn:schemas-upnp-org:service:ContentDirectory:1"},
	{"/serviceId", "urn:upnp-org:serviceId:ContentDirectory"},
	{"/controlURL", CONTENTDIRECTORY_CONTROLURL},
	{"/eventSubURL", CONTENTDIRECTORY_EVENTURL},
	{"/SCPDURL", CONTENTDIRECTORY_PATH},
	{"/serviceType", "urn:schemas-upnp-org:service:ConnectionManager:1"},
	{"/serviceId", "urn:upnp-org:serviceId:ConnectionManager"},
	{"/controlURL", CONNECTIONMGR_CONTROLURL},
	{"/eventSubURL", CONNECTIONMGR_EVENTURL},
	{"/SCPDURL", CONNECTIONMGR_PATH},
	{"/serviceType", "urn:microsoft.com:service:X_MS_MediaReceiverRegistrar:1"},
	{"/serviceId", "urn:microsoft.com:serviceId:X_MS_MediaReceiverRegistrar"},
	{"/controlURL", X_MS_MEDIARECEIVERREGISTRAR_CONTROLURL},
	{"/eventSubURL", X_MS_MEDIARECEIVERREGISTRAR_EVENTURL},
	{"/SCPDURL", X_MS_MEDIARECEIVERREGISTRAR_PATH},
	{0, 0}
};

/* For ConnectionManager */
static const struct argument GetProtocolInfoArgs[] =
{
	{"Source", 2, 0},
	{"Sink", 2, 1},
	{NULL, 0, 0}
};

static const struct argument GetCurrentConnectionIDsArgs[] =
{
	{"ConnectionIDs", 2, 2},
	{NULL, 0, 0}
};

static const struct argument GetCurrentConnectionInfoArgs[] =
{
	{"ConnectionID", 1, 7},
	{"RcsID", 2, 9},
	{"AVTransportID", 2, 8},
	{"ProtocolInfo", 2, 6},
	{"PeerConnectionManager", 2, 4},
	{"PeerConnectionID", 2, 7},
	{"Direction", 2, 5},
	{"Status", 2, 3},
	{NULL, 0, 0}
};

static const struct action ConnectionManagerActions[] =
{
	{"GetProtocolInfo", GetProtocolInfoArgs}, /* R */
	{"GetCurrentConnectionIDs", GetCurrentConnectionIDsArgs}, /* R */
	{"GetCurrentConnectionInfo", GetCurrentConnectionInfoArgs}, /* R */
	{0, 0}
};

static const struct argument GetSearchCapabilitiesArgs[] =
{
	{"SearchCaps", 2, 11},
	{0, 0}
};

static const struct argument GetSortCapabilitiesArgs[] =
{
	{"SortCaps", 2, 12},
	{0, 0}
};

static const struct argument GetSystemUpdateIDArgs[] =
{
	{"Id", 2, 13},
	{0, 0}
};

static const struct argument UpdateObjectArgs[] =
{
	{"ObjectID", 1, 1},
	{"CurrentTagValue", 1, 10},
	{"NewTagValue", 1, 10},
	{0, 0}
};

static const struct argument BrowseArgs[] =
{
	{"ObjectID", 1, 1},
	{"BrowseFlag", 1, 4},
	{"Filter", 1, 5},
	{"StartingIndex", 1, 7},
	{"RequestedCount", 1, 8},
	{"SortCriteria", 1, 6},
	{"Result", 2, 2},
	{"NumberReturned", 2, 8},
	{"TotalMatches", 2, 8},
	{"UpdateID", 2, 9},
	{0, 0}
};

static const struct argument SearchArgs[] =
{
	{"ContainerID", 1, 1},
	{"SearchCriteria", 1, 3},
	{"Filter", 1, 5},
	{"StartingIndex", 1, 7},
	{"RequestedCount", 1, 8},
	{"SortCriteria", 1, 6},
	{"Result", 2, 2},
	{"NumberReturned", 2, 8},
	{"TotalMatches", 2, 8},
	{"UpdateID", 2, 9},
	{0, 0}
};

static const struct action ContentDirectoryActions[] =
{
	{"GetSearchCapabilities", GetSearchCapabilitiesArgs}, /* R */
	{"GetSortCapabilities", GetSortCapabilitiesArgs}, /* R */
	{"GetSystemUpdateID", GetSystemUpdateIDArgs}, /* R */
	{"Browse", BrowseArgs}, /* R */
	{"Search", SearchArgs}, /* O */
	{"UpdateObject", UpdateObjectArgs}, /* O */
	{0, 0}
};

static const struct argument GetIsAuthorizedArgs[] =
{
	{"DeviceID", 1, 0},
	{"Result", 2, 3},
	{NULL, 0, 0}
};

static const struct argument GetIsValidatedArgs[] =
{
	{"DeviceID", 1, 0},
	{"Result", 2, 3},
	{NULL, 0, 0}
};

static const struct argument GetRegisterDeviceArgs[] =
{
	{"RegistrationReqMsg", 1, 1},
	{"RegistrationRespMsg", 2, 2},
	{NULL, 0, 0}
};

static const struct action X_MS_MediaReceiverRegistrarActions[] =
{
	{"IsAuthorized", GetIsAuthorizedArgs}, /* R */
	{"IsValidated", GetIsValidatedArgs}, /* R */
	{"RegisterDevice", GetRegisterDeviceArgs},
	{0, 0}
};
