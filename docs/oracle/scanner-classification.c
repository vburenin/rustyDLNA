/* MiniDLNA media server
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
 */
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <unistd.h>
#include <dirent.h>
#include <locale.h>
#include <libgen.h>
	media_types mtype = get_media_type(name);
	struct stat st;
	int have_inode = (stat(path, &st) == 0 && S_ISREG(st.st_mode) && st.st_ino);

	if (have_inode)
	{
		src_id = find_detail_by_inode((int64_t)st.st_dev, (int64_t)st.st_ino);
		if (src_id)
		{
			int64_t src_ts = sql_get_int64_field(db,
				"SELECT TIMESTAMP from DETAILS where ID = %lld", src_id);
			time_t side = (mtype == TYPE_VIDEO) ? video_sidecar_mtime(path) : 0;
			if (src_ts < (int64_t)st.st_mtime || src_ts < (int64_t)side)
				src_id = 0;
		}
	}

	if( mtype == TYPE_IMAGE && (types & TYPE_IMAGE) )
	{
		if( is_album_art(name) )
			return -1;
		strcpy(base, IMAGE_DIR_ID);
		class = "item.imageItem.photo";
		if (src_id)
			detailID = clone_detail_for_path(src_id, path,
				(int64_t)st.st_size, (int64_t)st.st_mtime,
				(int64_t)st.st_dev, (int64_t)st.st_ino);
		if (!detailID)
			detailID = GetImageMetadata(path, name);
	}
	else if( mtype == TYPE_VIDEO && (types & TYPE_VIDEO) )
	{
		strcpy(base, VIDEO_DIR_ID);
		class = "item.videoItem";
		if (src_id)
			detailID = clone_detail_for_path(src_id, path,
				(int64_t)st.st_size, (int64_t)st.st_mtime,
				(int64_t)st.st_dev, (int64_t)st.st_ino);
		if (!detailID)
			detailID = GetVideoMetadata(path, name);
	}
	else if( mtype == TYPE_PLAYLIST && (types & TYPE_PLAYLIST) )
	{
		if( insert_playlist(path, name) == 0 )
			return 1;
	}
	/* Some file extensions can be used for both audio and video.
	** Fall back to audio on these files if video parsing fails. */
	if (!detailID && (types & TYPE_AUDIO) && is_audio(name) )
	{
		strcpy(base, MUSIC_DIR_ID);
		class = "item.audioItem.musicTrack";
		if (src_id)
			detailID = clone_detail_for_path(src_id, path,
				(int64_t)st.st_size, (int64_t)st.st_mtime,
				(int64_t)st.st_dev, (int64_t)st.st_ino);
		if (!detailID)
			detailID = GetAudioMetadata(path, name);
	}
	if( !detailID )
	{
		DPRINTF(E_WARN, L_SCANNER, "Unsuccessful getting details for %s\n", path);
		return -1;
	}
	if (have_inode)
	{
		stamp_detail_inode(detailID, (int64_t)st.st_dev, (int64_t)st.st_ino);
		sync_inode_aliases(detailID);
	}

	snprintf(objectID, sizeof(objectID), "%s%s$%X", BROWSEDIR_ID, parentID, object);
	objname = strdup(name);
	strip_ext(objname);

	sql_exec(db, "INSERT into OBJECTS"
	             " (OBJECT_ID, PARENT_ID, CLASS, DETAIL_ID, NAME) "

