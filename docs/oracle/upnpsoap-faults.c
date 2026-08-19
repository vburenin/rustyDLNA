/* MiniDLNA project
 *
 * http://sourceforge.net/projects/minidlna/
 *
 * MiniDLNA media server
 * Copyright (C) 2008-2017  Justin Maggard
 *
 * This file is part of MiniDLNA.
 *
 * MiniDLNA is free software; you can redistribute it and/or modify
 * it under the terms of the GNU General Public License version 2 as
 * published by the Free Software Foundation.
 *
 * MiniDLNA is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with MiniDLNA. If not, see <http://www.gnu.org/licenses/>.
 *
 * Portions of the code from the MiniUPnP project:
 *
 * Copyright (c) 2006-2007, Thomas Bernard
#endif
#define NON_ZERO(x) (x && atoi(x))
#define IS_ZERO(x) (!x || !atoi(x))

/* Standard Errors:
 *
 * errorCode errorDescription Description
 * --------	---------------- -----------
 * 401 		Invalid Action 	No action by that name at this service.
 * 402 		Invalid Args 	Could be any of the following: not enough in args,
 * 							too many in args, no in arg by that name,
 * 							one or more in args are of the wrong data type.
 * 403 		Out of Sync 	Out of synchronization.
 * 501 		Action Failed 	May be returned in current state of service
 * 							prevents invoking that action.
 * 600-699 	TBD 			Common action errors. Defined by UPnP Forum
 * 							Technical Committee.
 * 700-799 	TBD 			Action-specific errors for standard actions.
 * 							Defined by UPnP Forum working committee.
 * 800-899 	TBD 			Action-specific errors for non-standard actions.
 * 							Defined by UPnP vendor.
*/
#define SoapError(x,y,z) _SoapError(x,y,z,__func__)
static void
_SoapError(struct upnphttp * h, int errCode, const char * errDesc, const char *func)
{
	static const char resp[] =
		"<s:Envelope "
		"xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" "
		"s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">"
		"<s:Body>"
		"<s:Fault>"
		"<faultcode>s:Client</faultcode>"
		"<faultstring>UPnPError</faultstring>"
		"<detail>"
		"<UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">"
		"<errorCode>%d</errorCode>"
		"<errorDescription>%s</errorDescription>"
		"</UPnPError>"
		"</detail>"
		"</s:Fault>"
		"</s:Body>"
		"</s:Envelope>";

	char body[2048];
	int bodylen;

	DPRINTF(E_WARN, L_HTTP, "%s Returning UPnPError %d: %s\n", func, errCode, errDesc);
	bodylen = snprintf(body, sizeof(body), resp, errCode, errDesc);
	BuildResp2_upnphttp(h, 500, "Internal Server Error", body, bodylen);
	SendResp_upnphttp(h);
	FinishResp_upnphttp(h);
}
				ret = xasprintf(&orderBy, "order by o.CLASS, d.TITLE");
			else
				orderBy = parse_sort_criteria(SortCriteria, &ret);
			if( ret == -1 )
			{
				free(orderBy);
				orderBy = NULL;
				ret = 0;
			}
		}
		/* If it's a DLNA client, return an error for bad sort criteria */
		if( ret < 0 && ((args.flags & FLAG_DLNA) || GETFLAG(DLNA_STRICT_MASK)) )
		{
			SoapError(h, 709, "Unsupported or invalid sort criteria");
			goto browse_error;
		}

		sql = sqlite3_mprintf("SELECT %s, %s, %s, " COLUMNS
				      "from OBJECTS o left join DETAILS d on (d.ID = o.DETAIL_ID)"
				      " where %s %s limit %d, %d;",
				      objectid_sql, parentid_sql, refid_sql,
	                                     " where (OBJECT_ID glob '%q%s') and (%s))"
	                                     " + "
	                                     "(select count(*) from OBJECTS o left join DETAILS d on (o.DETAIL_ID = d.ID)"
	                                     " where (OBJECT_ID = '%q') and (%s))",
	                                     ContainerID, sep, where, ContainerID, where);
	if( totalMatches < 0 )
	{
		/* Must be invalid SQL, so most likely bad or unhandled search criteria. */
		SoapError(h, 708, "Unsupported or invalid search criteria");
		goto search_error;
	}
	/* Does the object even exist? */
	if( !totalMatches )
	{
		if( !object_exists(ContainerID) )
		{
			SoapError(h, 710, "No such container");
			goto search_error;
		}
	}
	ret = 0;
	__SORT_LIMIT
	orderBy = parse_sort_criteria(SortCriteria, &ret);
	/* If it's a DLNA client, return an error for bad sort criteria */
	if( ret < 0 && ((args.flags & FLAG_DLNA) || GETFLAG(DLNA_STRICT_MASK)) )
	{
		SoapError(h, 709, "Unsupported or invalid sort criteria");
		goto search_error;
	}

	sql = sqlite3_mprintf( SELECT_COLUMNS
	                      "from OBJECTS o left join DETAILS d on (d.ID = o.DETAIL_ID)"
	                      " where OBJECT_ID glob '%q%s' and (%s) %s "
			if(strncmp(p, soapMethods[i].methodName, len) == 0)
			{
				soapMethods[i].methodImpl(h, soapMethods[i].methodName);
				return;
			}
			i++;
		}

		DPRINTF(E_WARN, L_HTTP, "SoapMethod: Unknown: %.*s\n", methodlen, p);
	}

	SoapError(h, 401, "Invalid Action");
}

/* Representative Browse/Search call sites from the same reference file. */
SoapError(h, 402, "Invalid Args");
SoapError(h, 701, "No such object error");
SoapError(h, 708, "Unsupported or invalid search criteria");
SoapError(h, 709, "Unsupported or invalid sort criteria");
