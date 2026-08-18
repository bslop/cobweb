/* draw.c — the smallest thing that puts jcc68k-generated pixels on a Jaguar.
 *
 * One Object Processor bitmap object over a 320x120 16bpp framebuffer, plus a
 * STOP. No interrupts, no double buffering, no GPU: the 68000 fills the buffer
 * once, programs the display, and halts. Everything visible is computed by
 * compiled C, so if it draws, the compiler produced working graphical code.
 *
 * Register layout and the object-list field packing follow the OpenLara Jaguar
 * port's video.c, which is the known-good reference for this hardware.
 * Jaguar RGB16 is R<<11 | B<<6 | G<<1 — green is the odd one out.
 */

#define REG16(a) (*(volatile unsigned short *)(a))
#define REG32(a) (*(volatile unsigned       *)(a))

#define VC    REG16(0xF00006)   /* vertical count, HALF-LINES; bit 11 = field! */
#define OLP   REG32(0xF00020)   /* object list pointer — halves swapped! */
#define OBF   REG16(0xF00026)   /* object processor flag — write 0 after OLP */
#define VMODE REG16(0xF00028)
#define BORD1 REG16(0xF0002A)
#define BORD2 REG16(0xF0002C)
#define CONFIG REG16(0xF00036)  /* bit 4: 1 = NTSC */
#define HDB1  REG16(0xF00038)
#define HDB2  REG16(0xF0003A)
#define HDE   REG16(0xF0003C)
#define VDB   REG16(0xF00046)
#define VDE   REG16(0xF00048)
#define BG    REG16(0xF00058)

#define W       320
#define H       120
#define BASE_X  16
/* YPOS = 2*BASE_Y half-lines must clear VDB on BOTH standards, or the object
 * starts before the display window opens and its top lines are clipped: VDB is
 * 25 on NTSC but 35 on PAL, so BASE_Y=16 (YPOS=32) is fine on NTSC hardware and
 * loses the top border under PAL. 20 -> YPOS=40, clear of both.
 * And YPOS + 2*HEIGHT must FIT THE FIELD: at BASE_Y=24 a 240-line object needs
 * 48 + 480 = 528 half-lines against a 524-half-line field, so its last TWO lines
 * fall off the end and the bottom border silently vanishes. 40 + 480 = 520 fits.
 * Same family as "HEIGHT is a count, not an index" in the shared notes. */
#define BASE_Y  20   /* see below */
#define PWIDTH  ((W * 2) / 8)        /* phrases per line at 16bpp */
#define DEPTH16 (4u << 12)           /* OBDEPTH 4 = 16bpp */

static unsigned short fb[W * H] __attribute__((aligned(16)));
static unsigned op_list[8] __attribute__((aligned(16)));

/* Jaguar RGB16: R at bits 15-11, B at 10-6, GREEN at 5-0 — six bits, unshifted.
 *
 * Measured, not assumed. Writing green as `g << 1` puts it in bits 1-6, which
 * both drops its low bit and spills its top bit into the blue field: a ramp fed
 * 0..63 comes back with 32 distinct green levels and 2 distinct blue, instead
 * of 63 and 1. That formula is safe only for a 5-BIT green, where nothing
 * reaches bit 6 — so it silently wastes the sixth bit rather than failing, and
 * every channel-isolation and ramp assertion still passes.
 *
 * r, b: 0..31.   g: 0..63.
 */
static unsigned short rgb(unsigned r, unsigned g, unsigned b)
{
    return (unsigned short)((r << 11) | (b << 6) | g);
}

/* Two ramps, deliberately ASYMMETRIC. Red and blue are 5-bit fields; green is
 * 6. Feeding 0..31 to all three means nothing ever exercises green's sixth bit,
 * so a layout packing 5-bit green into the 6-bit field passes every isolation
 * and ramp assertion — the check could discriminate, it was just never shown a
 * case that provoked the fault. Coverage and discrimination are separate
 * properties and the test needs both. */
static unsigned char ramp5[W];     /* x -> 0..31, for red and blue */
static unsigned char ramp6[W];     /* x -> 0..63, so green's low bit matters */

static void paint(void)
{
    int x, y;
    /* One divide per COLUMN, not per pixel: the 68000 has no 32-bit divide, so
       every `/` in an inner loop is a __divsi3 call. Dividing per pixel meant
       paint() had not finished after 60 frames. */
    for (x = 0; x < W; x++) {
        ramp5[x] = (unsigned char)((x * 31) / W);
        ramp6[x] = (unsigned char)((x * 63) / W);
    }

    for (y = 0; y < H; y++) {
        unsigned short *row = &fb[y * W];
        int band = (y < H / 3) ? 0 : ((y < (2 * H) / 3) ? 1 : 2);
        int diag = (y * W) / H;
        for (x = 0; x < W; x++) {
            unsigned v5 = ramp5[x], v6 = ramp6[x];
            unsigned short c;
            if (y < 2 || y >= H - 2 || x < 2 || x >= W - 2) c = rgb(31, 63, 31);
            else if (x == diag)                            c = rgb(31, 63, 0);
            else if (band == 0)                            c = rgb(v5, 0, 0);
            else if (band == 1)                            c = rgb(0, v6, 0);   /* 6-bit */
            else                                           c = rgb(0, 0, v5);
            row[x] = c;
        }
    }
    for (y = 20; y < 50; y++)
        for (x = 40; x < 70; x++)
            fb[y * W + x] = rgb(31, 0, 31);
}

/* ☠ THE OP DESTROYS THIS LIST EVERY FIELD, so it has to be rebuilt — building
 * it once and spinning draws exactly ONE field and then goes blank forever.
 * Hardware-confirmed 2026-08-17; jsim renders a build-once ROM perfectly, which
 * is why this survived so long. */
static void build_list(void)
{
    unsigned fb_addr = (unsigned)fb;
    unsigned link    = ((unsigned)&op_list[4]) >> 3;   /* STOP lives at [4] */

    /* One TYPE-0 bitmap object, then a STOP object. */
    op_list[0] = (fb_addr << 8) | (link >> 8);
    op_list[1] = (link << 24) | ((unsigned)H << 14) | ((unsigned)BASE_Y << 4);
    op_list[2] = PWIDTH >> 4;
    op_list[3] = ((unsigned)PWIDTH << 28)
               | ((unsigned)PWIDTH << 18)
               | (1u << 15)                            /* PITCH 1 */
               | DEPTH16
               | BASE_X;
    op_list[4] = 0;                                    /* STOP */
    op_list[5] = 4;
}

int main(void)
{
    unsigned fb_addr = (unsigned)fb;
    unsigned olp;
    int ntsc;
    unsigned width, hmid, height, vmid;

    paint();
    build_list();

    ntsc   = (CONFIG & 0x10) != 0;
    width  = ntsc ? 1409u : 1381u;
    hmid   = ntsc ?  823u :  843u;
    height = ntsc ?  241u :  287u;
    vmid   = ntsc ?  266u :  322u;

    unsigned short HDB1_v = (unsigned short)(hmid - width / 2u + 4u);
    unsigned short VDB_v  = (unsigned short)(vmid - height);
    HDE  = (unsigned short)((width / 2u - 1u) | 0x400u);
    HDB1 = HDB1_v;
    HDB2 = HDB1_v;
    VDB  = VDB_v;
    VDE  = 0xFFFF;
    BORD1 = 0;
    BORD2 = 0;
    BG    = 0;

    olp = (unsigned)op_list;
    OLP = (olp >> 16) | (olp << 16);                   /* halves swapped */
    OBF = 0;                       /* MANDATORY after every OLP write */

    /* Published BEFORE VIDEN. Video registers are write-only so reading them
       back proves nothing, and jag_resident measured that DRAM traffic during
       ACTIVE VIDEO starves the OP's per-field fetch — the picture bounces
       vertically with a tear line, and jsim renders it perfectly even at
       --fidelity silicon. Twelve longs is not a long burst, but there is no
       reason for ANY of it to race the beam. */
    {
        volatile unsigned *dbg = (volatile unsigned *)0x100000u;
        dbg[0] = 0x44425547u;            /* 'DBUG' */
        dbg[1] = fb_addr;
        dbg[2] = olp;
        dbg[3] = op_list[0];
        dbg[4] = op_list[1];
        dbg[5] = op_list[2];
        dbg[6] = op_list[3];
        dbg[7] = (unsigned)ntsc;
        dbg[8] = (unsigned)HDB1_v;
        dbg[9] = (unsigned)VDB_v;
        dbg[10] = (unsigned)fb[2 * W + 10];   /* a painted pixel */
        dbg[11] = (unsigned)fb[40 * W + 50];  /* inside the magenta square */
    }

    VMODE = 0x06C7;                                    /* VIDEN last */

    /* Rebuild ONCE PER FIELD, inside the vertical blank.
     *
     * ☠ NOT free-running. The OP ADVANCES the object's DATA pointer as it
     * renders each line, so a loop that rewrites the header continuously resets
     * DATA to the top of the framebuffer on every scanline and the whole screen
     * becomes a copy of LINE 0. An earlier revision of this file recommended
     * free-running; that was wrong, and it survived because neither test card
     * could see it — a solid fill is invariant in y, and this card's bands vary
     * only in x. ⭐ A card can only detect a line-stride fault if it varies
     * along the axis the fault acts on.
     *
     * ☠ MASK VC WITH $7FF. Bit 11 of VC ($F00006) is the FIELD flag, not part of
     * the half-line count, so comparing raw VC makes the gate always-true on
     * every second field: only alternate fields get a rebuilt list, and a
     * capture then averages object with background 50/50 — a floor of the BG
     * colour under every pixel, which reads exactly like a colour bug. Measured
     * on this card's ground colour: (0,0,125) unmasked -> (0,0,21) masked,
     * against rgb(0,0,3) as painted. */
    {
        /* Gate on VDB: rebuild in the blanking window at the TOP of the
           field, immediately before display starts — the same window
           OpenLara's vertical interrupt fires in.
           ☠ Do NOT derive the gate from the object height (vdb + 2*H + 4).
           For any object shorter than the display that lands MID-FIELD: the OP
           finishes the object, the rebuild resets DATA, and the OP draws the
           whole thing AGAIN over the rest of the field. The second pass is what
           you see, it starts wherever the rebuild landed, so YPOS appears to do
           nothing — measured with H=120, BASE_Y=16 and BASE_Y=100 both produced
           capture rows 251..473. A full-height object hides it.
           ☠ Nor from the display end (vmid + height). That is 609 half-lines on
           PAL, longer than the field, so the wait never completes and the list
           is never rebuilt at all — which is exactly how jsim (which reports
           PAL) rendered nothing while the NTSC hardware was fine.
           And NOT VDB either: VDB is 35 on PAL while this object's YPOS is
           2*BASE_Y = 32, so a "rebuild while VC < VDB" window OVERLAPS the
           object's first fetch and clobbers DATA as the OP starts drawing —
           which clipped exactly the four border edges while every band, ramp
           and the magenta block still passed.
           Rebuild just after the field WRAPS instead: VC 0..7 is inside the
           field on both standards and finishes well before YPOS=32. OpenLara
           works to the same constraint from the other side, firing its VI at
           vdb-4 against a hard ~350us deadline. */
        unsigned blank = 8u;
        for (;;) {
            while ((unsigned)(VC & 0x7FF) >= blank) { }  /* wait for vblank  */
            build_list();
            OLP = (olp >> 16) | (olp << 16);
            OBF = 0;
            while ((unsigned)(VC & 0x7FF) <  blank) { }  /* one per field    */
        }
    }
}
