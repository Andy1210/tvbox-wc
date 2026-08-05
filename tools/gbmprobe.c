// Does GBM on this hardware accept the formats/modifiers the Pi's video decoder
// produces? Smithay's ExportFramebuffer imports a client dmabuf into GBM
// (gbm_bo_import with GBM_BO_USE_SCANOUT) before calling drmModeAddFB2, while
// wlroots goes from the dmabuf fds straight to drmModeAddFB2WithModifiers. If GBM
// refuses SAND128, that alone explains why a Smithay compositor can never put the
// Pi's decoded video on a plane.
//
//   gbmprobe /dev/dri/card1

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <gbm.h>
#include <drm_fourcc.h>

#ifndef DRM_FORMAT_MOD_BROADCOM_SAND128
#define DRM_FORMAT_MOD_BROADCOM_SAND128 fourcc_mod_broadcom_code(4, 0)
#endif

static const char *fmt_name(uint32_t f) {
	static char b[5];
	memcpy(b, &f, 4);
	b[4] = 0;
	return b;
}

static void probe(struct gbm_device *g, const char *node, uint32_t fmt,
		uint64_t mod, const char *modname) {
	int planes = gbm_device_get_format_modifier_plane_count(g, fmt, mod);
	printf("  %-8s %-4s + %-22s plane_count=%-3d", node, fmt_name(fmt), modname, planes);

	struct gbm_bo *bo = gbm_bo_create_with_modifiers2(g, 1920, 1080, fmt, &mod, 1,
		GBM_BO_USE_SCANOUT);
	printf("  create_with_modifiers(SCANOUT)=%s", bo ? "OK" : "FAIL");
	if (bo) gbm_bo_destroy(bo);

	// The path Smithay's importer would take is gbm_bo_import; creating is the
	// closest proxy we can run without a decoded frame in hand.
	bo = gbm_bo_create(g, 1920, 1080, fmt, GBM_BO_USE_SCANOUT);
	printf("  create(no modifier)=%s\n", bo ? "OK" : "FAIL");
	if (bo) gbm_bo_destroy(bo);
}

int main(int argc, char **argv) {
	const char *nodes[] = { argc > 1 ? argv[1] : "/dev/dri/card1", "/dev/dri/renderD128" };
	for (unsigned n = 0; n < sizeof(nodes) / sizeof(*nodes); n++) {
		int fd = open(nodes[n], O_RDWR | O_CLOEXEC);
		if (fd < 0) { printf("%s: cannot open\n", nodes[n]); continue; }
		struct gbm_device *g = gbm_create_device(fd);
		if (!g) { printf("%s: gbm_create_device failed\n", nodes[n]); close(fd); continue; }
		printf("%s (gbm backend: %s)\n", nodes[n], gbm_device_get_backend_name(g));
		const char *base = strrchr(nodes[n], '/') + 1;
		probe(g, base, DRM_FORMAT_NV12, DRM_FORMAT_MOD_BROADCOM_SAND128, "BROADCOM_SAND128");
		probe(g, base, DRM_FORMAT_P030, DRM_FORMAT_MOD_BROADCOM_SAND128, "BROADCOM_SAND128");
		probe(g, base, DRM_FORMAT_NV12, DRM_FORMAT_MOD_LINEAR, "LINEAR");
		probe(g, base, DRM_FORMAT_XRGB8888, DRM_FORMAT_MOD_LINEAR, "LINEAR");
		gbm_device_destroy(g);
		close(fd);
	}
	return 0;
}
