/* Small C wrapper so Rust never needs the layout of mbe_parms. */
#include <stdlib.h>
#include <string.h>
#include "mbelib.h"

typedef struct { mbe_parms cur, prev, prev_enhanced; } sdr_mbe;

/* Decoded parameters of the frame just processed (for the AGC). Vl is 1-indexed, valid for 1..L. */
typedef struct { float w0; int L; int repeat; int Vl[57]; } sdr_frame_info;

void *sdr_mbe_new(void) {
    sdr_mbe *s = calloc(1, sizeof *s);
    if (s) mbe_initMbeParms(&s->cur, &s->prev, &s->prev_enhanced);
    return s;
}

void sdr_mbe_free(void *s) { free(s); }

/* One deinterleaved IMBE 7200x4400 frame -> 160 float samples at 8 kHz (mbelib scale, before its x7 output gain).
   Returns corrected bit errors (errs2). */
int sdr_mbe_imbe(void *p, char frame[8][23], float out[160], sdr_frame_info *info) {
    sdr_mbe *s = p;
    int errs = 0, errs2 = 0;
    char err_str[64] = {0};
    char imbe_d[88];
    mbe_processImbe7200x4400Framef(out, &errs, &errs2, err_str, frame, imbe_d, &s->cur, &s->prev, &s->prev_enhanced, 3);
    info->w0 = s->cur.w0;
    info->L = s->cur.L;
    info->repeat = s->cur.repeat;
    memcpy(info->Vl, s->cur.Vl, sizeof info->Vl);
    return errs2;
}
