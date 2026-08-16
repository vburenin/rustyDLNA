/* HTTP/1.0 and 1.1 persist rules (also used by src/test_improvements.c). */
#ifndef HTTPERSIST_H
#define HTTPERSIST_H

#include <string.h>

/* HTTP/1.1 stays open unless the client sent Connection: close.
 * HTTP/1.0 stays open only with an explicit Connection: keep-alive.
 * close always wins if both tokens are present. */
static inline int
http_should_persist(const char *httpver, int conn_close, int conn_keep)
{
	if (!httpver || conn_close)
		return 0;
	if (strcmp(httpver, "HTTP/1.1") == 0)
		return 1;
	if (strcmp(httpver, "HTTP/1.0") == 0)
		return conn_keep;
	return 0;
}

#endif
