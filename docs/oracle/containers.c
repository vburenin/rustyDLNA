/* MiniDLNA media server
 * Copyright (C) 2014 NETGEAR
 *
 * GPL-2.0 excerpt from src/containers.c. It locks virtual-view IDs and
 * Samsung/PlaysForSure aliases used by the differential tests.
 */
#define NINETY_DAYS "7776000"

const char *music_id = MUSIC_ID;
const char *music_all_id = MUSIC_ALL_ID;
const char *music_genre_id = MUSIC_GENRE_ID;
const char *music_artist_id = MUSIC_ARTIST_ID;
const char *music_album_id = MUSIC_ALBUM_ID;
const char *music_plist_id = MUSIC_PLIST_ID;
const char *music_dir_id = MUSIC_DIR_ID;
const char *video_id = VIDEO_ID;
const char *video_all_id = VIDEO_ALL_ID;
const char *video_dir_id = VIDEO_DIR_ID;
const char *image_id = IMAGE_ID;
const char *image_all_id = IMAGE_ALL_ID;
const char *image_date_id = IMAGE_DATE_ID;
const char *image_camera_id = IMAGE_CAMERA_ID;
const char *image_dir_id = IMAGE_DIR_ID;

struct magic_container_s magic_containers[] =
{
	{ "Recently Added", "1$FF0", NULL, "\"1$FF0$\" || OBJECT_ID",
	  "\"1$FF0\"", "o.OBJECT_ID", NULL,
	  "MIME glob 'a*' and REF_ID is NULL", "order by TIMESTAMP DESC", 50, 0 },
	{ "Recently Added", "2$FF0", NULL, "\"2$FF0$\" || OBJECT_ID",
	  "\"2$FF0\"", "o.OBJECT_ID", NULL,
	  "MIME glob 'v*' and REF_ID is NULL", "order by TIMESTAMP DESC", 50, 0 },
	{ "Recently Added", "3$FF0", NULL, "\"3$FF0$\" || OBJECT_ID",
	  "\"3$FF0\"", "o.OBJECT_ID", NULL,
	  "MIME glob 'i*' and REF_ID is NULL", "order by TIMESTAMP DESC", 50, 0 },
	{ NULL, "4", &music_all_id, NULL, NULL, NULL, NULL, NULL, NULL, -1, FLAG_MS_PFS },
	{ NULL, "5", &music_genre_id, NULL, NULL, NULL, NULL, NULL, NULL, -1, FLAG_MS_PFS },
	{ NULL, "6", &music_artist_id, NULL, NULL, NULL, NULL, NULL, NULL, -1, FLAG_MS_PFS },
	{ NULL, "7", &music_album_id, NULL, NULL, NULL, NULL, NULL, NULL, -1, FLAG_MS_PFS },
	{ NULL, "8", &video_all_id, NULL, NULL, NULL, NULL, NULL, NULL, -1, FLAG_MS_PFS },
	{ NULL, "B", &image_all_id, NULL, NULL, NULL, NULL, NULL, NULL, -1, FLAG_MS_PFS },
	{ NULL, "C", &image_date_id, NULL, NULL, NULL, NULL, NULL, NULL, -1, FLAG_MS_PFS },
	{ NULL, "F", &music_plist_id, NULL, NULL, NULL, NULL, NULL, NULL, -1, FLAG_MS_PFS },
	{ NULL, "14", &music_dir_id, NULL, NULL, NULL, NULL, NULL, NULL, -1, FLAG_MS_PFS },
	{ NULL, "15", &video_dir_id, NULL, NULL, NULL, NULL, NULL, NULL, -1, FLAG_MS_PFS },
	{ NULL, "16", &image_dir_id, NULL, NULL, NULL, NULL, NULL, NULL, -1, FLAG_MS_PFS },
	{ NULL, "D2", &image_camera_id, NULL, NULL, NULL, NULL, NULL, NULL, -1, FLAG_MS_PFS },
	{ NULL, "I", &image_id, NULL, NULL, NULL, NULL, NULL, NULL, -1, FLAG_SAMSUNG_DCM10 },
	{ NULL, "A", &music_id, NULL, NULL, NULL, NULL, NULL, NULL, -1, FLAG_SAMSUNG_DCM10 },
	{ NULL, "V", &video_id, NULL, NULL, NULL, NULL, NULL, NULL, -1, FLAG_SAMSUNG_DCM10 },
};
