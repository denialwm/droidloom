// SurfaceFlinger's video path, headless: a decoder-style YUV AHardwareBuffer,
// and an RGBA one as the path every other buffer takes, filled by the CPU with four quadrants (red, green, blue, white), imported
// with eglCreateImageKHR(EGL_NATIVE_BUFFER_ANDROID), sampled through
// samplerExternalOES into an RGBA target and read back. Each format runs in
// its own child so an abort reports instead of ending the run. Opens no
// window; tools/droidloom-update/tests/gpu_yuv_sample.rs builds and runs it.
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>
#include <linux/dma-buf.h>

typedef struct { uint32_t width, height, layers, format; uint64_t usage; uint32_t stride, rfu0; uint64_t rfu1; } Desc;
typedef struct AHardwareBuffer AHardwareBuffer;
typedef struct { int version, fds, ints, data[]; } NativeHandle;
int AHardwareBuffer_allocate(const Desc *, AHardwareBuffer **);
const NativeHandle *AHardwareBuffer_getNativeHandle(const AHardwareBuffer *);
void AHardwareBuffer_release(AHardwareBuffer *);

typedef void *EGLDisplay, *EGLConfig, *EGLContext, *EGLImage, *EGLClientBuffer;
typedef int32_t EGLint;
typedef unsigned int EGLenum;
typedef unsigned GLenum, GLuint;
typedef int GLint;
#define EGL_NONE 0x3038
#define EGL_NATIVE_BUFFER_ANDROID 0x3140
#define EGL_IMAGE_PRESERVED_KHR 0x30D2
#define EGL_RENDERABLE_TYPE 0x3040
#define EGL_OPENGL_ES2_BIT 4
#define EGL_SURFACE_TYPE 0x3033
#define EGL_PBUFFER_BIT 1
#define EGL_CONTEXT_CLIENT_VERSION 0x3098
#define GL_TEXTURE_2D 0x0DE1
#define GL_TEXTURE_EXTERNAL_OES 0x8D65
#define GL_FRAMEBUFFER 0x8D40
#define GL_COLOR_ATTACHMENT0 0x8CE0
#define GL_RGBA 0x1908
#define GL_UNSIGNED_BYTE 0x1401
#define GL_TEXTURE_MIN_FILTER 0x2801
#define GL_TEXTURE_MAG_FILTER 0x2800
#define GL_NEAREST 0x2600
#define GL_VERTEX_SHADER 0x8B31
#define GL_FRAGMENT_SHADER 0x8B30
#define GL_COMPILE_STATUS 0x8B81
#define GL_LINK_STATUS 0x8B82
#define GL_TRIANGLE_STRIP 5
#define GL_FLOAT 0x1406
#define GL_FRAMEBUFFER_COMPLETE 0x8CD5

static void *(*gpa)(const char *);
typedef void (*DebugProc)(EGLenum, const char *, EGLint, void *, void *, const char *);
static EGLint (*debug_control)(DebugProc, const intptr_t *);
static void debug(EGLenum error, const char *command, EGLint type, void *thread, void *object, const char *message) {
    printf("[EGL %#x %s: %s] ", error, command, message ? message : "");
}
#define F(ret, name, ...) static ret (*name)(__VA_ARGS__);
F(EGLDisplay, eglGetDisplay, void *)
F(int, eglInitialize, EGLDisplay, EGLint *, EGLint *)
F(int, eglChooseConfig, EGLDisplay, const EGLint *, EGLConfig *, EGLint, EGLint *)
F(EGLContext, eglCreateContext, EGLDisplay, EGLConfig, EGLContext, const EGLint *)
F(int, eglMakeCurrent, EGLDisplay, void *, void *, EGLContext)
F(EGLint, eglGetError, void)
F(EGLImage, eglCreateImageKHR, EGLDisplay, EGLContext, GLenum, EGLClientBuffer, const EGLint *)
F(EGLClientBuffer, eglGetNativeClientBufferANDROID, const AHardwareBuffer *)
F(void, glEGLImageTargetTexture2DOES, GLenum, EGLImage)
F(void, glGenTextures, int, GLuint *)
F(void, glBindTexture, GLenum, GLuint)
F(void, glTexParameteri, GLenum, GLenum, GLint)
F(void, glTexImage2D, GLenum, GLint, GLint, int, int, GLint, GLenum, GLenum, const void *)
F(void, glGenFramebuffers, int, GLuint *)
F(void, glBindFramebuffer, GLenum, GLuint)
F(void, glFramebufferTexture2D, GLenum, GLenum, GLenum, GLuint, GLint)
F(GLenum, glCheckFramebufferStatus, GLenum)
F(void, glViewport, int, int, int, int)
F(void, glReadPixels, int, int, int, int, GLenum, GLenum, void *)
F(GLenum, glGetError, void)
F(GLuint, glCreateShader, GLenum)
F(void, glShaderSource, GLuint, int, const char *const *, const GLint *)
F(void, glCompileShader, GLuint)
F(void, glGetShaderiv, GLuint, GLenum, GLint *)
F(void, glGetShaderInfoLog, GLuint, int, int *, char *)
F(GLuint, glCreateProgram, void)
F(void, glAttachShader, GLuint, GLuint)
F(void, glLinkProgram, GLuint)
F(void, glGetProgramiv, GLuint, GLenum, GLint *)
F(void, glUseProgram, GLuint)
F(void, glBindAttribLocation, GLuint, GLuint, const char *)
F(void, glVertexAttribPointer, GLuint, GLint, GLenum, unsigned char, int, const void *)
F(void, glEnableVertexAttribArray, GLuint)
F(void, glDrawArrays, GLenum, GLint, int)
F(void, glFinish, void)
#define LOAD(name) do { name = gpa(#name); if (!name) { printf("no %s\n", #name); return 1; } } while (0)

enum { W = 64, H = 64 };
// Quadrants: top-left red, top-right green, bottom-left blue, bottom-right white.
static const uint8_t RGB[4][3] = {{255, 0, 0}, {0, 255, 0}, {0, 0, 255}, {255, 255, 255}};

// Narrow range, in 8-bit units: BT.601, or BT.2020 for the 10-bit layout,
// the models the driver suggests.
static void to_yuv(const uint8_t *rgb, int bt2020, double *y, double *u, double *v) {
    double kr = bt2020 ? 0.2627 : 0.299, kb = bt2020 ? 0.0593 : 0.114;
    double r = rgb[0] / 255.0, g = rgb[1] / 255.0, b = rgb[2] / 255.0;
    double l = kr * r + (1 - kr - kb) * g + kb * b;
    *y = 16 + 219 * l;
    *u = 128 + 224 * (b - l) / (2 * (1 - kb));
    *v = 128 + 224 * (r - l) / (2 * (1 - kr));
}

static int quadrant(int x, int y) { return (y >= H / 2) * 2 + (x >= W / 2); }

#define FOURCC(a, b, c, d) ((uint32_t)(a) | (uint32_t)(b) << 8 | (uint32_t)(c) << 16 | (uint32_t)(d) << 24)

// Frames rotate the colours between quadrants, as a decoder writes new
// content into a buffer the consumer already imported.
static int frame;
static const uint8_t *colour(int q) { return RGB[(q + frame) % 4]; }

// The CPU fill goes straight into the DMA-BUF at minigbm's plane layout: a
// probe in /data cannot load the vendor mapper that lockPlanes needs.
static int fill(AHardwareBuffer *buffer) {
    const NativeHandle *h = AHardwareBuffer_getNativeHandle(buffer);
    const int *w = h->data;  // minigbm words: descriptors, then integers
    uint32_t fourcc = (uint32_t)w[20], planes = (uint32_t)w[31];
    if (!frame) {
        printf("%.4s planes %u modifier %#llx ", (const char *)&fourcc, planes,
               (unsigned long long)((uint64_t)(uint32_t)w[23] << 32 | (uint32_t)w[22]));
        for (uint32_t i = 0; i < planes; i++) printf("[%u+%u] ", w[9 + i], w[5 + i]);
    }
    // Byte offsets of each sample: Y, then the chroma pair.
    int interleaved = 0, u_plane = 0, v_plane = 0, u_first = 0, wide = 0, rgba = 0;
    switch (fourcc) {
    case FOURCC('A', 'B', '2', '4'): rgba = 1; break;
    case FOURCC('P', '0', '1', '0'): interleaved = 1; u_plane = v_plane = 1; u_first = 1; wide = 1; break;
    case FOURCC('N', 'V', '1', '2'): interleaved = 1; u_plane = v_plane = 1; u_first = 1; break;
    case FOURCC('N', 'V', '2', '1'): interleaved = 1; u_plane = v_plane = 1; u_first = 0; break;
    case FOURCC('9', '9', '9', '7'): // YVU420_ANDROID: YV12 with 16-aligned chroma stride
    case FOURCC('Y', 'V', '1', '2'): interleaved = 0; u_plane = 2; v_plane = 1; u_first = 0; break;
    case FOURCC('Y', 'U', '1', '2'): interleaved = 0; u_plane = 1; v_plane = 2; u_first = 0; break;
    default: printf("unknown layout"); return 1;
    }
    int fd = h->data[0];
    off_t size = lseek(fd, 0, SEEK_END);
    uint8_t *map = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (map == MAP_FAILED) { printf("mmap failed"); return 1; }
    struct dma_buf_sync sync = {DMA_BUF_SYNC_START | DMA_BUF_SYNC_WRITE};
    ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync);
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++) {
            if (rgba) {
                uint8_t *pixel = map + w[9] + y * w[5] + x * 4;
                memcpy(pixel, colour(quadrant(x, y)), 3);
                pixel[3] = 255;
                continue;
            }
            double Y, U, V;
            to_yuv(colour(quadrant(x, y)), wide, &Y, &U, &V);
            if (wide) {
                // P010: 10 bits in the top of each little-endian 16-bit sample.
                uint16_t *luma = (uint16_t *)(map + w[9] + y * w[5]) + x;
                *luma = (uint16_t)((int)(Y * 4 + 0.5) << 6);
                if (x % 2 || y % 2) continue;
                uint16_t *pair = (uint16_t *)(map + w[9 + 1] + y / 2 * w[5 + 1]) + x;
                pair[0] = (uint16_t)((int)(U * 4 + 0.5) << 6);
                pair[1] = (uint16_t)((int)(V * 4 + 0.5) << 6);
                continue;
            }
            map[w[9] + y * w[5] + x] = (uint8_t)(Y + 0.5);
            if (x % 2 || y % 2) continue;
            int cx = x / 2, cy = y / 2;
            if (interleaved) {
                uint8_t *pair = map + w[9 + 1] + cy * w[5 + 1] + cx * 2;
                pair[0] = (uint8_t)((u_first ? U : V) + 0.5);
                pair[1] = (uint8_t)((u_first ? V : U) + 0.5);
            } else {
                map[w[9 + u_plane] + cy * w[5 + u_plane] + cx] = (uint8_t)(U + 0.5);
                map[w[9 + v_plane] + cy * w[5 + v_plane] + cx] = (uint8_t)(V + 0.5);
            }
        }
    sync.flags = DMA_BUF_SYNC_END | DMA_BUF_SYNC_WRITE;
    ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync);
    munmap(map, size);
    return 0;
}

static GLuint shader(GLenum kind, const char *source) {
    GLuint s = glCreateShader(kind);
    glShaderSource(s, 1, &source, NULL);
    glCompileShader(s);
    GLint ok = 0;
    glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[512]; glGetShaderInfoLog(s, sizeof log, NULL, log); printf("shader: %s", log); exit(3); }
    return s;
}

static int run(const char *name, uint32_t format) {
    printf("%-8s ", name);
    EGLDisplay display = eglGetDisplay(0);
    EGLint major, minor, n;
    eglInitialize(display, &major, &minor);
    const EGLint config_attributes[] = {EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT, EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_NONE};
    EGLConfig config;
    eglChooseConfig(display, config_attributes, &config, 1, &n);
    const EGLint context_attributes[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, 0, context_attributes);
    eglMakeCurrent(display, 0, 0, context);

    // GPU_SAMPLED_IMAGE, as a decoder's output buffer.
    Desc desc = {W, H, 1, format, 1ull << 8, 0, 0, 0};
    AHardwareBuffer *buffer = NULL;
    if (AHardwareBuffer_allocate(&desc, &buffer)) { printf("unsupported\n"); return 0; }
    if (fill(buffer)) { printf("\n"); return 1; }

    if (debug_control) debug_control(debug, NULL);
    const EGLint image_attributes[] = {EGL_IMAGE_PRESERVED_KHR, 1, EGL_NONE};
    EGLImage image = eglCreateImageKHR(display, 0, EGL_NATIVE_BUFFER_ANDROID,
                                       eglGetNativeClientBufferANDROID(buffer), image_attributes);
    if (!image) { printf("eglCreateImage 0x%x\n", eglGetError()); return 1; }
    GLuint source, target, framebuffer;
    glGenTextures(1, &source);
    glBindTexture(GL_TEXTURE_EXTERNAL_OES, source);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glEGLImageTargetTexture2DOES(GL_TEXTURE_EXTERNAL_OES, image);
    if (glGetError()) { printf("image target failed\n"); return 1; }

    glGenTextures(1, &target);
    glBindTexture(GL_TEXTURE_2D, target);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, W, H, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, target, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) { printf("framebuffer incomplete\n"); return 1; }

    GLuint program = glCreateProgram();
    glAttachShader(program, shader(GL_VERTEX_SHADER,
        "attribute vec2 p; varying vec2 t;"
        "void main() { t = p * 0.5 + 0.5; gl_Position = vec4(p, 0.0, 1.0); }"));
    glAttachShader(program, shader(GL_FRAGMENT_SHADER,
        "#extension GL_OES_EGL_image_external : require\n"
        "precision mediump float; varying vec2 t; uniform samplerExternalOES s;"
        "void main() { gl_FragColor = texture2D(s, t); }"));
    glBindAttribLocation(program, 0, "p");
    glLinkProgram(program);
    glUseProgram(program);
    static const float quad[] = {-1, -1, 1, -1, -1, 1, 1, 1};
    glVertexAttribPointer(0, 2, GL_FLOAT, 0, 0, quad);
    glEnableVertexAttribArray(0);
    glViewport(0, 0, W, H);
    // Frame 0 as imported; frame 1 rewritten under the same texture, as
    // SurfaceFlinger keeps one per buffer; frame 2 rewritten and rebound.
    int failed = 0;
    for (frame = 0; frame < 3; frame++) {
        if (frame) fill(buffer);
        if (frame == 2) glEGLImageTargetTexture2DOES(GL_TEXTURE_EXTERNAL_OES, image);
        glBindTexture(GL_TEXTURE_EXTERNAL_OES, source);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glFinish();

        // Texture row 0 is the buffer's first row and lands at framebuffer row 0.
        uint8_t pixels[W * H * 4];
        glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
        int bad_frame = 0;
        char seen[96] = "";
        for (int q = 0; q < 4; q++) {
            int x = (q % 2) * W / 2 + W / 4, y = (q / 2) * H / 2 + H / 4;
            const uint8_t *got = pixels + (y * W + x) * 4;
            for (int c = 0; c < 3; c++) bad_frame |= abs(got[c] - colour(q)[c]) > 8;
            snprintf(seen + strlen(seen), sizeof seen - strlen(seen), "%d,%d,%d ", got[0], got[1], got[2]);
        }
        printf("%s%s ", (const char *[]){"first", "rewritten", "rebound"}[frame], bad_frame ? " FAIL" : " ok");
        if (bad_frame) printf("(%s) ", seen);
        failed |= bad_frame;
    }
    printf("\n");
    AHardwareBuffer_release(buffer);
    return failed;
}

// Loaded in each child: libEGL starts threads, which fork does not copy.
static int load(void) {
    void *egl = dlopen("libEGL.so", RTLD_NOW);
    gpa = dlsym(egl, "eglGetProcAddress");
    LOAD(eglGetDisplay); LOAD(eglInitialize); LOAD(eglChooseConfig); LOAD(eglCreateContext); LOAD(eglMakeCurrent);
    LOAD(eglGetError); LOAD(eglCreateImageKHR); LOAD(eglGetNativeClientBufferANDROID); LOAD(glEGLImageTargetTexture2DOES);
    LOAD(glGenTextures); LOAD(glBindTexture); LOAD(glTexParameteri); LOAD(glTexImage2D); LOAD(glGenFramebuffers);
    LOAD(glBindFramebuffer); LOAD(glFramebufferTexture2D); LOAD(glCheckFramebufferStatus); LOAD(glViewport);
    LOAD(glReadPixels); LOAD(glGetError); LOAD(glCreateShader); LOAD(glShaderSource); LOAD(glCompileShader);
    LOAD(glGetShaderiv); LOAD(glGetShaderInfoLog); LOAD(glCreateProgram); LOAD(glAttachShader); LOAD(glLinkProgram);
    LOAD(glGetProgramiv); LOAD(glUseProgram); LOAD(glBindAttribLocation); LOAD(glVertexAttribPointer);
    LOAD(glEnableVertexAttribArray); LOAD(glDrawArrays); LOAD(glFinish);
    debug_control = gpa("eglDebugMessageControlKHR");
    return 0;
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    // Formats a decoder or camera hands to SurfaceFlinger.
    static const struct { const char *name; uint32_t format; } formats[] = {
        {"RGBA8888", 1},           // AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM, the path apps share
        {"YCbCr420", 0x23},        // AHARDWAREBUFFER_FORMAT_Y8Cb8Cr8_420 (flexible)
        {"YV12", 0x32315659},      // HAL_PIXEL_FORMAT_YV12
        {"NV21", 0x11},            // HAL_PIXEL_FORMAT_YCrCb_420_SP
        {"P010", 0x36},            // AHARDWAREBUFFER_FORMAT_YCbCr_P010 (HDR video)
    };
    int failed = 0;
    for (unsigned i = 0; i < sizeof formats / sizeof *formats; i++) {
        fflush(stdout);
        pid_t child = fork();
        if (child == 0) _exit(load() || run(formats[i].name, formats[i].format));
        int status = 0;
        waitpid(child, &status, 0);
        if (WIFSIGNALED(status)) { printf("CRASH signal %d\n", WTERMSIG(status)); failed = 1; }
        else failed |= WEXITSTATUS(status) != 0;
    }
    return failed;
}
