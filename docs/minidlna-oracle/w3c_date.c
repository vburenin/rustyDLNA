/*
 * Kodi's Platinum NPT_DateTime FORMAT_W3C parser rejects
 * YYYY-MM-DDTHH:MM:SS (19 chars): seconds require a timezone, so the
 * string must be at least 20 chars (e.g. ...SSZ). A failed parse
 * clears dc:date and Kodi shows year 1905.
 */
int
w3c_date_from_time(time_t t, char *buf, size_t buflen)
{
	struct tm *tm;

	if( !buf || buflen < 21 )
		return -1;
	tm = gmtime(&t);
	if( !tm || strftime(buf, buflen, "%Y-%m-%dT%H:%M:%SZ", tm) == 0 )
	{
		buf[0] = '\0';
		return -1;
	}
	return 0;
}

void
w3c_normalize_date(const char *date, char *buf, size_t buflen)
{
	size_t n;

	if( !buf || buflen == 0 )
		return;
	buf[0] = '\0';
	if( !date || !date[0] )
		return;

	n = strlen(date);

	/* Bare year from Kodi <year>1999</year> */
	if( n == 4 && date[0] >= '1' && date[0] <= '2' &&
	    isdigit((unsigned char)date[1]) && isdigit((unsigned char)date[2]) &&
	    isdigit((unsigned char)date[3]) )
	{
		if( buflen < 11 )
			return;
		memcpy(buf, date, 4);
		memcpy(buf + 4, "-01-01", 7);
		return;
	}

	/* YYYY-MM-DDTHH:MM:SS (no timezone) */
	if( n == 19 && date[4] == '-' && date[7] == '-' &&
	    (date[10] == 'T' || date[10] == ' ') &&
	    date[13] == ':' && date[16] == ':' )
	{
		if( buflen < 21 )
			return;
		memcpy(buf, date, 19);
		buf[10] = 'T';
		buf[19] = 'Z';
		buf[20] = '\0';
		return;
	}

	/* EXIF "YYYY:MM:DD HH:MM:SS" */
	if( n == 19 && date[4] == ':' && date[7] == ':' &&
	    date[10] == ' ' && date[13] == ':' && date[16] == ':' )
	{
		if( buflen < 21 )
			return;
		memcpy(buf, date, 19);
		buf[4] = '-';
		buf[7] = '-';
		buf[10] = 'T';
		buf[19] = 'Z';
		buf[20] = '\0';
		return;
	}

	strncpyt(buf, date, buflen);
}
