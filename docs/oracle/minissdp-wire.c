/* MiniDLNA media server
 * This file is part of MiniDLNA.
 *
 * The code herein is based on the MiniUPnP Project.
 * http://miniupnp.free.fr/ or http://miniupnp.tuxfamily.org/
 *
 * Copyright (c) 2006, Thomas Bernard
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
	}

	return s;
}

static const char * const known_service_types[] =
{
	uuidvalue,
	"upnp:rootdevice",
	"urn:schemas-upnp-org:device:MediaServer:",
	"urn:schemas-upnp-org:service:ContentDirectory:",
	"urn:schemas-upnp-org:service:ConnectionManager:",
	"urn:microsoft.com:service:X_MS_MediaReceiverRegistrar:",
	0
};

static void
_usleep(long min, long max)
{
	struct timespec sleep_time;
	long usecs = min + rand() / (RAND_MAX / (max - min + 1) + 1);

	sleep_time.tv_sec = 0;
	sleep_time.tv_nsec = usecs * 1000;
	nanosleep(&sleep_time, NULL);
}

/* not really an SSDP "announce" as it is the response
 * to a SSDP "M-SEARCH" */
static void
SendSSDPResponse(int s, struct sockaddr_in sockname, int st_no,
		 const char *host, unsigned short port, socklen_t len_r)
{
	int l, n;
	char buf[512];
	char tmstr[30];
	time_t tm = time(NULL);

	/*
	 * follow guideline from document "UPnP Device Architecture 1.0"
	 * uppercase is recommended.
	 * DATE: is recommended
	 * SERVER: OS/ver UPnP/1.0 minidlna/1.0
	 * - check what to put in the 'Cache-Control' header 
	 * */
	strftime(tmstr, sizeof(tmstr), "%a, %d %b %Y %H:%M:%S GMT", gmtime(&tm));
	l = snprintf(buf, sizeof(buf), "HTTP/1.1 200 OK\r\n"
		"CACHE-CONTROL: max-age=%u\r\n"
		"DATE: %s\r\n"
		"ST: %s%s\r\n"
		"USN: %s%s%s%s\r\n"
		"EXT:\r\n"
		"SERVER: " MINIDLNA_SERVER_STRING "\r\n"
		"LOCATION: http://%s:%u" ROOTDESC_PATH "\r\n"
		"Content-Length: 0\r\n"
		"\r\n",
		(runtime_vars.notify_interval<<1)+10,
		tmstr,
		known_service_types[st_no],
		(st_no > 1 ? "1" : ""),
		uuidvalue,
		(st_no > 0 ? "::" : ""),
		(st_no > 0 ? known_service_types[st_no] : ""),
		(st_no > 1 ? "1" : ""),
		host, (unsigned int)port);
	DPRINTF(E_DEBUG, L_SSDP, "Sending M-SEARCH response to %s:%d ST: %s\n",
		inet_ntoa(sockname.sin_addr), ntohs(sockname.sin_port),
		known_service_types[st_no]);
	n = sendto(s, buf, l, 0,
	           (struct sockaddr *)&sockname, len_r);
	if (n < 0)
		DPRINTF(E_ERROR, L_SSDP, "sendto(udp): %s\n", strerror(errno));
}

void
SendSSDPNotifies(int s, const char *host, unsigned short port,
                 unsigned int interval)
{
	struct sockaddr_in sockname;
	int l, n, dup, i=0;
	unsigned int lifetime;
	char bufr[512];

	memset(&sockname, 0, sizeof(struct sockaddr_in));
	sockname.sin_family = AF_INET;
	sockname.sin_port = htons(SSDP_PORT);
	sockname.sin_addr.s_addr = inet_addr(SSDP_MCAST_ADDR);
	lifetime = (interval << 1) + 10;

	for (dup = 0; dup < 2; dup++)
	{
		if (dup)
			_usleep(150000, 250000);
		i = 0;
		while (known_service_types[i])
		{
			l = snprintf(bufr, sizeof(bufr), 
					"NOTIFY * HTTP/1.1\r\n"
					"HOST:%s:%d\r\n"
					"CACHE-CONTROL:max-age=%u\r\n"
					"LOCATION:http://%s:%d" ROOTDESC_PATH"\r\n"
					"SERVER: " MINIDLNA_SERVER_STRING "\r\n"
					"NT:%s%s\r\n"
					"USN:%s%s%s%s\r\n"
					"NTS:ssdp:alive\r\n"
					"\r\n",
					SSDP_MCAST_ADDR, SSDP_PORT,
					lifetime,
					host, port,
					known_service_types[i],
					(i > 1 ? "1" : ""),
					uuidvalue,
					(i > 0 ? "::" : ""),
					(i > 0 ? known_service_types[i] : ""),
					(i > 1 ? "1" : ""));
			if (l >= sizeof(bufr))
			{
				DPRINTF(E_WARN, L_SSDP, "SendSSDPNotifies(): truncated output\n");
				l = sizeof(bufr);
			}
			DPRINTF(E_MAXDEBUG, L_SSDP, "Sending ssdp:alive [%d]\n", s);
			n = sendto(s, bufr, l, 0,
				(struct sockaddr *)&sockname, sizeof(struct sockaddr_in));
			if (n < 0)
				DPRINTF(E_ERROR, L_SSDP, "sendto(udp_notify=%d, %s): %s\n", s, host, strerror(errno));
			i++;
		}
	}
}

static void
/* This will broadcast ssdp:byebye notifications to inform 
 * the network that UPnP is going down. */
int
SendSSDPGoodbyes(int s)
{
	struct sockaddr_in sockname;
	int n, l;
	int i;
	int dup, ret = 0;
	char bufr[512];

	memset(&sockname, 0, sizeof(struct sockaddr_in));
	sockname.sin_family = AF_INET;
	sockname.sin_port = htons(SSDP_PORT);
	sockname.sin_addr.s_addr = inet_addr(SSDP_MCAST_ADDR);

	for (dup = 0; dup < 2; dup++)
	{
		for (i = 0; known_service_types[i]; i++)
		{
			l = snprintf(bufr, sizeof(bufr),
					"NOTIFY * HTTP/1.1\r\n"
					"HOST:%s:%d\r\n"
					"NT:%s%s\r\n"
					"USN:%s%s%s%s\r\n"
					"NTS:ssdp:byebye\r\n"
					"\r\n",
					SSDP_MCAST_ADDR, SSDP_PORT,
					known_service_types[i],
					(i > 1 ? "1" : ""), uuidvalue,
					(i > 0 ? "::" : ""),
					(i > 0 ? known_service_types[i] : ""),
					(i > 1 ? "1" : ""));
			DPRINTF(E_MAXDEBUG, L_SSDP, "Sending ssdp:byebye [%d]\n", s);
			n = sendto(s, bufr, l, 0,
			           (struct sockaddr *)&sockname, sizeof(struct sockaddr_in) );
			if (n < 0)
			{

