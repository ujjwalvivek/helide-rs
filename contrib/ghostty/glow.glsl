// Text glow for Ghostty, ported from Nib's: each glyph is stamped faintly around itself in three
// rings that fade outwards, in its own colour, so text gets a soft halo and the rest stays crisp.
//
// Use it in Ghostty's config (it applies to the whole terminal, so to helide too):
//
//   custom-shader = /path/to/helide-rs/contrib/ghostty/glow.glsl
//
// Made for dark themes: a pixel glows when it is brighter than the pixel it lands on.

// How strong the halo is, 0 to 1 (Nib's default strength, 60).
const float STRENGTH = 0.6;
// How far the halo reaches, in pixels: Nib's default spread is 30% of a line, and a line is
// about 20 px at font-size 11 on a 1x screen. Raise it on a HiDPI screen or with a larger font.
const float RADIUS = 6.0;

// Nib's three rings: how far out (of RADIUS), how many stamps, and each stamp's weight.
const vec3 RINGS[3] = vec3[3](vec3(0.30, 8.0, 0.20), vec3(0.65, 12.0, 0.11), vec3(1.00, 16.0, 0.06));

float luma(vec3 c) {
    return dot(c, vec3(0.2126, 0.7152, 0.0722));
}

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 px = 1.0 / iResolution.xy;
    vec4 base = texture(iChannel0, fragCoord * px);
    float here = luma(base.rgb);

    // Nib's stamps add up to a halo; here they add up as light, weighted by each ring.
    vec3 glow = vec3(0.0);
    for (int r = 0; r < 3; r++) {
        vec3 ring = RINGS[r];
        int stamps = int(ring.y);
        for (int k = 0; k < stamps; k++) {
            float angle = (float(k) + 0.5 * float(r)) * 6.283185307 / ring.y;
            vec2 at = fragCoord - vec2(cos(angle), sin(angle)) * RADIUS * ring.x;
            vec3 s = texture(iChannel0, at * px).rgb;
            // Only text glows, and brighter text more: dim comments barely do.
            float text = smoothstep(0.08, 0.5, luma(s) - here);
            glow += s * ring.z * text;
        }
    }
    // The rings' weights add up to 3.88; screen the halo over the pixel so it never clips.
    glow *= STRENGTH / 3.88;
    vec3 col = 1.0 - (1.0 - base.rgb) * (1.0 - glow);

    fragColor = vec4(col, base.a);
}
