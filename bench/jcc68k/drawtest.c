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

#define OLP   REG32(0xF00020)   /* object list pointer — halves swapped! */
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
#define BASE_Y  16
#define PWIDTH  ((W * 2) / 8)        /* phrases per line at 16bpp */
#define DEPTH16 (4u << 12)           /* OBDEPTH 4 = 16bpp */

static unsigned short fb[W * H] __attribute__((aligned(16)));
static unsigned op_list[8] __attribute__((aligned(16)));

static unsigned short rgb(unsigned r, unsigned g, unsigned b)
{
    return (unsigned short)((r << 11) | (b << 6) | (g << 1));
}

static unsigned char ramp[W];      /* x -> 0..31, computed once */

static void paint(void)
{
    int x, y;
    /* One divide per COLUMN, not per pixel: the 68000 has no 32-bit divide, so
       every `/` in an inner loop is a __divsi3 call. Dividing per pixel meant
       paint() had not finished after 60 frames. */
    for (x = 0; x < W; x++) ramp[x] = (unsigned char)((x * 31) / W);

    for (y = 0; y < H; y++) {
        unsigned short *row = &fb[y * W];
        int band = (y < H / 3) ? 0 : ((y < (2 * H) / 3) ? 1 : 2);
        int diag = (y * W) / H;
        for (x = 0; x < W; x++) {
            unsigned v = ramp[x];
            unsigned short c;
            if (y < 2 || y >= H - 2 || x < 2 || x >= W - 2) c = rgb(31, 31, 31);
            else if (x == diag)                            c = rgb(31, 31, 0);
            else if (band == 0)                            c = rgb(v, 0, 0);
            else if (band == 1)                            c = rgb(0, v, 0);
            else                                           c = rgb(0, 0, v);
            row[x] = c;
        }
    }
    for (y = 20; y < 50; y++)
        for (x = 40; x < 70; x++)
            fb[y * W + x] = rgb(31, 0, 31);
}

int main(void)
{
    unsigned fb_addr = (unsigned)fb;
    unsigned link    = ((unsigned)&op_list[4]) >> 3;   /* STOP lives at [4] */
    unsigned olp;
    int ntsc;
    unsigned width, hmid, height, vmid;

    paint();

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

    for (;;) { }
}
