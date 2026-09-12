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

/* Custom AVIO keeps all media reads on the admitted descriptor. The callbacks
 * and AVFormatContext fields are compiled against the installed libav headers,
 * including their FFmpeg 8 layout. pread preserves shared descriptor cursors. */
#include <errno.h>
#include <limits.h>
#include <sys/stat.h>
#include <unistd.h>
#include <libavutil/mem.h>

typedef struct RustyConfinedInput {
    int fd;
    int64_t position;
    unsigned denied_opens;
    AVIOInterruptCB interrupt;
} RustyConfinedInput;

static int confined_read(void *opaque, uint8_t *buffer, int size)
{
    RustyConfinedInput *input = opaque;
    ssize_t result;
    if (input->interrupt.callback != NULL && input->interrupt.callback(input->interrupt.opaque))
        return AVERROR_EXIT;
    if (size <= 0 || input->position > INT64_MAX - size)
        return AVERROR(EINVAL);
    do {
        result = pread(input->fd, buffer, size, input->position);
    } while (result < 0 && errno == EINTR);
    if (result < 0)
        return AVERROR(errno);
    if (result == 0)
        return AVERROR_EOF;
    input->position += result;
    return result;
}

static int64_t confined_seek(void *opaque, int64_t offset, int whence)
{
    RustyConfinedInput *input = opaque;
    struct stat metadata;
    int64_t base;
    if (fstat(input->fd, &metadata) < 0)
        return AVERROR(errno);
    if (whence == AVSEEK_SIZE)
        return metadata.st_size;
    whence &= ~AVSEEK_FORCE;
    switch (whence) {
    case SEEK_SET: base = 0; break;
    case SEEK_CUR: base = input->position; break;
    case SEEK_END: base = metadata.st_size; break;
    default: return AVERROR(EINVAL);
    }
    if ((offset > 0 && base > INT64_MAX - offset) ||
        (offset < 0 && offset < -base))
        return AVERROR(EINVAL);
    input->position = base + offset;
    return input->position;
}

static int reject_nested_open(AVFormatContext *context, AVIOContext **io,
                              const char *url, int flags, AVDictionary **options)
{
    RustyConfinedInput *input = context->opaque;
    if (input != NULL && input->denied_opens < UINT_MAX)
        input->denied_opens++;
    (void)io; (void)url; (void)flags; (void)options;
    return AVERROR(EACCES);
}

AVIOContext *rusty_dlna_confined_avio(AVFormatContext *context, int fd)
{
    RustyConfinedInput *input = av_mallocz(sizeof(*input));
    uint8_t *buffer = av_malloc(32768);
    AVIOContext *io;
    if (input == NULL || buffer == NULL) {
        av_free(input); av_free(buffer);
        return NULL;
    }
    input->fd = fd;
    input->interrupt = context->interrupt_callback;
    io = avio_alloc_context(buffer, 32768, 0, input, confined_read, NULL, confined_seek);
    if (io == NULL) {
        av_free(input); av_free(buffer);
        return NULL;
    }
    context->pb = io;
    context->opaque = input;
    context->flags |= AVFMT_FLAG_CUSTOM_IO;
    context->io_open = reject_nested_open;
    return io;
}

void rusty_dlna_free_confined_avio(AVIOContext **io)
{
    if (io != NULL && *io != NULL) {
        av_freep(&(*io)->opaque);
        av_freep(&(*io)->buffer);
        avio_context_free(io);
    }
}

unsigned rusty_dlna_confined_denied_opens(AVIOContext *io)
{
    RustyConfinedInput *input = io->opaque;
    return input->denied_opens;
}
