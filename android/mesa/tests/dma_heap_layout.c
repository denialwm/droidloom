/* Exercise the actual allocator and minigbm layout helpers, without a GPU. */
#include <assert.h>
#include "dma_heap.c"

static struct bo layout(uint32_t format, uint32_t width, uint32_t height)
{
    struct bo bo = {0};
    bo.meta.format = format;
    bo.meta.width = width;
    bo.meta.height = height;
    bo.meta.num_planes = drv_num_planes_from_format(format);
    assert(!dma_heap_image_compute_metadata(&bo, width, height, format, 0, NULL, 0));
    for (size_t p = 0; p < bo.meta.num_planes; ++p) {
        assert(bo.meta.strides[p] % 128 == 0);
        assert(bo.meta.strides[p] >= drv_stride_from_format(format, width, p));
        assert(bo.meta.offsets[p] % 128 == 0);
        assert(bo.meta.offsets[p] + bo.meta.sizes[p] <= bo.meta.total_size);
        if (p) assert(bo.meta.offsets[p] >= bo.meta.offsets[p-1] + bo.meta.sizes[p-1]);
    }
    return bo;
}

int main(void)
{
    /* Reported 576x1024 video: 576/288/288 pitches fail Turnip import. */
    struct bo video = layout(DRM_FORMAT_YVU420_ANDROID, 576, 1024);
    assert(video.meta.strides[0] == 768);
    assert(video.meta.strides[1] == 384 && video.meta.strides[2] == 384);
    for (uint32_t width = 2; width <= 4096; width += 2) {
        for (uint32_t height = 2; height <= 6; height += 2) {
            struct bo yv12 = layout(DRM_FORMAT_YVU420_ANDROID, width, height);
            assert(yv12.meta.strides[1] == ALIGN(yv12.meta.strides[0] / 2, 16));
            assert(yv12.meta.sizes[0] == yv12.meta.strides[0] * height);
            layout(DRM_FORMAT_YVU420, width, height);
            layout(DRM_FORMAT_NV12, width, height);
            layout(DRM_FORMAT_P010, width, height);
            struct bo rgb = layout(DRM_FORMAT_ABGR8888, width, height);
            assert(rgb.meta.strides[0] == ALIGN(width * 4, 128));
        }
    }
    puts("PASS: planar video pitches, Android YV12 layout, nonoverlapping planes, RGB/NV12/P010");
    return 0;
}
