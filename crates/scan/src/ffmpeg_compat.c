#include <libavformat/avformat.h>

/*
 * FFmpeg 6 and newer expose demuxer-level coded side data through
 * AVCodecParameters.
 * Keep the header-version distinction in C instead of making the Rust scanner
 * guess the layout of the FFmpeg structs. Older libraries fall through to the
 * existing extradata parser; no deprecated AVStream API is used.
 */
const uint8_t *rusty_dlna_codec_side_data(
    AVCodecParameters *parameters,
    enum AVPacketSideDataType type,
    size_t *size)
{
#if LIBAVCODEC_VERSION_MAJOR >= 60
    const AVPacketSideData *side_data;

    if (parameters == NULL || size == NULL) {
        return NULL;
    }
    side_data = av_packet_side_data_get(
        parameters->coded_side_data,
        parameters->nb_coded_side_data,
        type);
    if (side_data != NULL) {
        *size = side_data->size;
        return side_data->data;
    }
    *size = 0;
    return NULL;
#else
    (void)parameters;
    (void)type;
    if (size != NULL) {
        *size = 0;
    }
    return NULL;
#endif
}
