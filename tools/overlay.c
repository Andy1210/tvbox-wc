// Fullscreen translucent layer-shell client, EGL/GLES2 so the buffer is a dmabuf
// (an shm buffer can never be handed to a KMS plane). Mimics the tvbox shell's
// transparent fullscreen UI sitting above the video.
//
//   overlay [-a] [-l overlay|top] [-o alpha]
//     -a         repaint continuously (default: paint once and stay quiescent)
//     -l LAYER   layer-shell layer (default: overlay)
//     -o ALPHA   0.0 - 1.0 fill alpha (default 0.35)

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <wayland-client.h>
#include <wayland-egl.h>
#include <EGL/egl.h>
#include <GLES2/gl2.h>
#include "wlr-layer-shell-unstable-v1-client-protocol.h"

static struct wl_display *display;
static struct wl_compositor *compositor;
static struct zwlr_layer_shell_v1 *layer_shell;
static struct wl_output *output;
static struct wl_surface *surface;
static struct zwlr_layer_surface_v1 *layer_surface;
static struct wl_egl_window *egl_window;
static EGLDisplay egl_display;
static EGLContext egl_context;
static EGLSurface egl_surface;

static int width, height, configured, running = 1, animate = 0;
static float fill_alpha = 0.35f;
static uint32_t layer = ZWLR_LAYER_SHELL_V1_LAYER_OVERLAY;

static void registry_global(void *data, struct wl_registry *reg, uint32_t name,
		const char *iface, uint32_t ver) {
	if (!strcmp(iface, wl_compositor_interface.name))
		compositor = wl_registry_bind(reg, name, &wl_compositor_interface, 4);
	else if (!strcmp(iface, zwlr_layer_shell_v1_interface.name))
		layer_shell = wl_registry_bind(reg, name, &zwlr_layer_shell_v1_interface, 1);
	else if (!strcmp(iface, wl_output_interface.name) && !output)
		output = wl_registry_bind(reg, name, &wl_output_interface, 2);
}
static void registry_remove(void *d, struct wl_registry *r, uint32_t n) {}
static const struct wl_registry_listener registry_listener = {
	registry_global, registry_remove };

static void draw(void) {
	static float t = 0.0f;
	glViewport(0, 0, width, height);
	// Premultiplied alpha: the colour channels must not exceed alpha.
	glClearColor(0.05f * fill_alpha, 0.10f * fill_alpha, 0.25f * fill_alpha, fill_alpha);
	glClear(GL_COLOR_BUFFER_BIT);
	if (animate) {
		// A moving opaque bar, so it is obvious on screen that we are repainting.
		int bar = (int)(t) % (width > 200 ? width - 200 : 1);
		glEnable(GL_SCISSOR_TEST);
		glScissor(bar, height / 2 - 40, 200, 80);
		glClearColor(0.9f, 0.9f, 0.9f, 1.0f);
		glClear(GL_COLOR_BUFFER_BIT);
		glDisable(GL_SCISSOR_TEST);
		t += 6.0f;
	}
	eglSwapBuffers(egl_display, egl_surface);
}

static void ls_configure(void *data, struct zwlr_layer_surface_v1 *ls,
		uint32_t serial, uint32_t w, uint32_t h) {
	zwlr_layer_surface_v1_ack_configure(ls, serial);
	width = w ? (int)w : 1920;
	height = h ? (int)h : 1080;
	if (!egl_window) {
		egl_window = wl_egl_window_create(surface, width, height);
		egl_surface = eglCreateWindowSurface(egl_display, (EGLConfig)data,
			(EGLNativeWindowType)egl_window, NULL);
		eglMakeCurrent(egl_display, egl_surface, egl_surface, egl_context);
		eglSwapInterval(egl_display, 1);
	} else {
		wl_egl_window_resize(egl_window, width, height, 0, 0);
	}
	configured = 1;
	fprintf(stderr, "overlay: configured %dx%d\n", width, height);
	draw();
}
static void ls_closed(void *data, struct zwlr_layer_surface_v1 *ls) { running = 0; }
static const struct zwlr_layer_surface_v1_listener ls_listener = {
	ls_configure, ls_closed };

int main(int argc, char **argv) {
	int opt;
	while ((opt = getopt(argc, argv, "al:o:")) != -1) {
		if (opt == 'a') animate = 1;
		else if (opt == 'o') fill_alpha = strtof(optarg, NULL);
		else if (opt == 'l' && !strcmp(optarg, "top"))
			layer = ZWLR_LAYER_SHELL_V1_LAYER_TOP;
	}

	display = wl_display_connect(NULL);
	if (!display) { fprintf(stderr, "overlay: no wayland display\n"); return 1; }
	struct wl_registry *registry = wl_display_get_registry(display);
	wl_registry_add_listener(registry, &registry_listener, NULL);
	wl_display_roundtrip(display);
	if (!compositor || !layer_shell) {
		fprintf(stderr, "overlay: compositor=%p layer_shell=%p - missing global\n",
			(void*)compositor, (void*)layer_shell);
		return 1;
	}

	egl_display = eglGetDisplay((EGLNativeDisplayType)display);
	eglInitialize(egl_display, NULL, NULL);
	eglBindAPI(EGL_OPENGL_ES_API);
	EGLint cfg_attr[] = {
		EGL_SURFACE_TYPE, EGL_WINDOW_BIT,
		EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
		EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
		EGL_ALPHA_SIZE, 8,          // an alpha channel: this is what makes it ARGB
		EGL_NONE };
	EGLConfig config; EGLint n = 0;
	if (!eglChooseConfig(egl_display, cfg_attr, &config, 1, &n) || n < 1) {
		fprintf(stderr, "overlay: no ARGB EGLConfig\n"); return 1;
	}
	EGLint ctx_attr[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
	egl_context = eglCreateContext(egl_display, config, EGL_NO_CONTEXT, ctx_attr);

	surface = wl_compositor_create_surface(compositor);
	layer_surface = zwlr_layer_shell_v1_get_layer_surface(layer_shell, surface,
		output, layer, "tvbox-overlay-test");
	zwlr_layer_surface_v1_set_anchor(layer_surface,
		ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP | ZWLR_LAYER_SURFACE_V1_ANCHOR_BOTTOM |
		ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT | ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT);
	zwlr_layer_surface_v1_set_size(layer_surface, 0, 0);   // whole output
	zwlr_layer_surface_v1_set_exclusive_zone(layer_surface, -1);
	zwlr_layer_surface_v1_set_keyboard_interactivity(layer_surface, 0);
	zwlr_layer_surface_v1_add_listener(layer_surface, &ls_listener, (void*)config);
	wl_surface_commit(surface);

	while (running && wl_display_dispatch(display) != -1) {
		if (configured && animate) draw();
	}
	return 0;
}
