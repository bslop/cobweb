/* main.c — Cobweb jsim calibration harness (68000 side).
 *
 * Runs the GPU timing probes from probes.s, one kernel at a time, in two
 * modes: A = 68k busy-polling the DRAM done flag (bus noise present), B = 68k
 * STOPped between vertical interrupts (quiet bus). The A/B delta measures 68k
 * bus contention directly.
 *
 * Raw results (VC start/end/wraps per probe+mode) land in a DRAM table at
 * 0x100000 — readable via `jagemu peek` in the emulator — and, in the skunk
 * build, are printed over the Skunkboard USB console (`jcp -c`). All math
 * happens host-side in parse_results.py; the 68k only moves and prints raw
 * values.
 *
 * GPU-in-main probes run LAST and in mode A only: if the JUMP-in-DRAM bug
 * workarounds fail on real silicon, the GPU wedges and only a power cycle
 * recovers it (bug 23: no external master can clear GO).
 */

typedef unsigned long u32;
typedef unsigned short u16;

#define R16(a) (*(volatile u16 *)(a))
#define R32(a) (*(volatile u32 *)(a))

#define VI_REG 0xF0004EUL /* NOT $F0000E — that yeti define is wrong on HW */
#define INT1 0xF000E0UL
#define G_PC 0xF02110UL
#define G_CTRL 0xF02114UL
#define G_RAM 0xF03000UL

#define PARAM_REPS (G_RAM + 0xF80)
#define PARAM_RESULT (G_RAM + 0xF84)
#define PARAM_AUX (G_RAM + 0xF88)
#define SRAM_SCRATCH (G_RAM + 0xFC0)

#define HDR 0x100000UL
#define RESULTS 0x100010UL
#define DRAM_BUF 0x140000UL /* 256 KB seeded read/write buffer */
#define DRAM_CODE 0x060000UL /* long-aligned staging base for main-RAM bodies */
#define MAGIC_DONE 0xC0DED04EUL

extern void cpu_stop(void);
extern void irq_on(void);

#ifdef USE_SKUNK_CONSOLE
/* tursilion skunklib via corpus-proven glue (skunk.s + skunkglue.s):
 * init does RESET + 2x NOP to sync both EZ-Host buffers with the PC. */
extern void skunk_init(void);
extern void skunk_puts(const char *str);
extern void skunk_close(void);
#endif

extern char p_vcmod_s[], p_vcmod_e[];
extern char p_null_s[], p_null_e[];
extern char p_nop_s[], p_nop_e[];
extern char p_move_s[], p_move_e[];
extern char p_moveq_s[], p_moveq_e[];
extern char p_adddep_s[], p_adddep_e[];
extern char p_addind_s[], p_addind_e[];
extern char p_ldsram_s[], p_ldsram_e[];
extern char p_ldidx_s[], p_ldidx_e[];
extern char p_lddram_s[], p_lddram_e[];
extern char p_lddramc_s[], p_lddramc_e[];
extern char p_ldstride_s[], p_ldstride_e[];
extern char p_stdram_s[], p_stdram_e[];
extern char p_blitsm_s[], p_blitsm_e[];
extern char p_blitbg_s[], p_blitbg_e[];
extern char p_dsphammer_s[], p_dsphammer_e[];
extern char p_divhot_s[], p_divhot_e[];
extern char p_divsh_s[], p_divsh_e[];
extern char p_jr_s[], p_jr_e[];
extern char p_main_s[], p_main_e[];
extern char pm_bodymov_s[], pm_bodymov_e[];
extern char pm_bodynop_s[], pm_bodynop_e[];

struct probe {
    const char *name; /* 8 chars, padded — keeps console lines aligned */
    char *ks, *ke;    /* kernel copied to GPU SRAM */
    char *bs, *be;    /* optional body staged to DRAM_CODE */
    u32 reps;
    u32 aux;
    char *ds, *de;    /* optional DSP hammer: run on Jerry concurrently */
    int op;           /* 1 = run with the OP scanning a full-screen bitmap */
    int bonly;        /* 1 = mode B only (mode A's busy-poll would be starved) */
};

#define D_RAM 0xF1B000UL
#define D_PC 0xF1A110UL
#define D_CTRL 0xF1A114UL

/* ── Object Processor load (scan-out contention probe) ────────────────────────
 * The OP is the one master that reads DRAM *continuously* — every displayed
 * line, all frame — and it outranks the GPU. This ROM normally sets no
 * VMODE/OLP at all, so the OP is idle; enabling a full-screen BITMAP gives a
 * clean on/off contrast to time Tom's DRAM stream against.
 * 320x240 @ 16bpp = 80 phrases/line = the heaviest steady OP demand available,
 * so a null result here is conclusive for 8bpp too. */
#define OP_FB 0x00180000UL   /* bitmap pixels (just past DRAM_BUF's 256 KB) */
#define OP_LIST 0x001AE000UL /* BITMAP -> STOP object list */
#define OLP_R 0xF00020UL

/* NEVER touch VMODE. An earlier version wrote VMODE=0 to stop the OP and wedged
 * the console (red boot screen, physical power-cycle to recover): this ROM sets
 * no VMODE of its own — it inherits the boot value — and its mode-B probes sleep
 * in cpu_stop() until the VERTICAL INTERRUPT. Killing video timing kills the VI,
 * the 68k never wakes, and the suite hangs mid-run.
 *
 * So vary only the OP's *appetite*, by swapping which object list OLP points at:
 * a full-screen 320x240 16bpp BITMAP (80 phrases/line, every displayed line) vs
 * a bare STOP (the OP fetches one object and quits). Video timing, the VI, and
 * the display mode are all untouched. Worst case the screen shows garbage. */
static void op_display(int on)
{
    u32 ol = OP_LIST, pw = (320u * 2) / 8; /* phrases per line, 16bpp */
    u32 link = (ol + 16) >> 3;
    if (on) {
        R32(ol) = (OP_FB << 8) | (link >> 8);
        R32(ol + 4) = (link << 24) | (240u << 14); /* height=240, ypos=0 */
        R32(ol + 8) = pw >> 4;
        R32(ol + 12) = (pw << 28) | (pw << 18) | (1u << 15) | (4u << 12);
        R32(ol + 16) = 0; /* STOP */
        R32(ol + 20) = 4;
    } else {
        R32(ol) = 0; /* STOP immediately — OP fetches one object, no pixels */
        R32(ol + 4) = 4;
    }
    R32(OLP_R) = (ol >> 16) | (ol << 16); /* word-swapped, as hardware */
}

static const struct probe probes[] = {
    { "vcmod   ", p_vcmod_s, p_vcmod_e, 0, 0, 0x00080000UL, 0 },
    { "null    ", p_null_s, p_null_e, 0, 0, 8192, 0 },
    { "nop     ", p_nop_s, p_nop_e, 0, 0, 1024, 0 },
    { "move    ", p_move_s, p_move_e, 0, 0, 1024, 0 },
    { "moveq   ", p_moveq_s, p_moveq_e, 0, 0, 1024, 0 },
    { "adddep  ", p_adddep_s, p_adddep_e, 0, 0, 1024, 0 },
    { "addind  ", p_addind_s, p_addind_e, 0, 0, 1024, 0 },
    { "ldsram  ", p_ldsram_s, p_ldsram_e, 0, 0, 512, SRAM_SCRATCH },
    { "ldidx   ", p_ldidx_s, p_ldidx_e, 0, 0, 512, SRAM_SCRATCH },
    { "lddram  ", p_lddram_s, p_lddram_e, 0, 0, 512, DRAM_BUF },
    { "lddramc ", p_lddramc_s, p_lddramc_e, 0, 0, 256, DRAM_BUF },
    { "ldstride", p_ldstride_s, p_ldstride_e, 0, 0, 256, DRAM_BUF },
    { "stdram  ", p_stdram_s, p_stdram_e, 0, 0, 512, DRAM_BUF },
    { "blitsm  ", p_blitsm_s, p_blitsm_e, 0, 0, 128, DRAM_BUF },
    { "blitbg  ", p_blitbg_s, p_blitbg_e, 0, 0, 128, DRAM_BUF },
    /* mode B ONLY: a saturating OP starves the 68k's mode-A DRAM busy-poll
     * and the suite hangs (observed on hardware). Mode B sleeps on the VI, so
     * it cannot be starved — and it is the cleaner measurement regardless. */
    { "lddramop", p_lddram_s, p_lddram_e, 0, 0, 512, DRAM_BUF, 0, 0, 1, 1 },
    /* lddramj (Tom stream + concurrent DSP hammer) is RETIRED from the default
     * run: it answered its question (Jerry does not measurably contend with Tom
     * — 656 vs 656 ticks mode B, twice, with DSPMARK proving the DSP ran), and
     * saturating the shared bus from Jerry wedged the console hard enough to
     * need a power-cycle (red boot screen). Re-enable deliberately, never as
     * part of a routine bench.
     * { "lddramj ", p_lddram_s, p_lddram_e, 0, 0, 512, DRAM_BUF, p_dsphammer_s, p_dsphammer_e }, */
    { "divhot  ", p_divhot_s, p_divhot_e, 0, 0, 512, 0 },
    { "divsh   ", p_divsh_s, p_divsh_e, 0, 0, 512, 0 },
    { "jr      ", p_jr_s, p_jr_e, 0, 0, 256, 0 },
    { "mainmov ", p_main_s, p_main_e, pm_bodymov_s, pm_bodymov_e, 128, 0 },
    { "mainnop ", p_main_s, p_main_e, pm_bodynop_s, pm_bodynop_e, 128, 0 },
};
#define NPROBES ((int)(sizeof(probes) / sizeof(probes[0])))
/* Session 1 proved the GPU-in-main probes safe on this silicon (Owl/Scavone
 * alignment rules held), so session 2 runs them in both modes. They stay
 * LAST in the table so a wedge on other silicon still loses nothing. */

static void copy16(u32 dst, const char *s, const char *e)
{
    volatile u16 *d = (volatile u16 *)dst;
    const u16 *p = (const u16 *)s;
    while ((const char *)p < e)
        *d++ = *p++;
}

/* ── console helpers (raw hex; all real math is host-side) ───────────────── */

static char line[96];

static char *put(char *d, const char *s)
{
    while (*s)
        *d++ = *s++;
    return d;
}

static char *puth(char *d, u32 v)
{
    static const char h[] = "0123456789ABCDEF";
    int i;
    for (i = 28; i >= 0; i -= 4)
        *d++ = h[(v >> i) & 15];
    return d;
}

static void say(const char *s)
{
#ifdef USE_SKUNK_CONSOLE
    skunk_puts(s);
#else
    (void)s;
#endif
}

/* ── probe execution ─────────────────────────────────────────────────────── */

static u32 slot_of(int idx, int mode)
{
    return RESULTS + ((u32)idx << 5) + ((u32)mode << 4);
}

static int run_probe(int idx, int mode)
{
    const struct probe *p = &probes[idx];
    u32 res = slot_of(idx, mode);
    u32 guard;

    R32(res + 12) = 0;
    copy16(G_RAM, p->ks, p->ke);
    if (p->bs)
        copy16(DRAM_CODE, p->bs, p->be);
    R32(PARAM_REPS) = p->reps;
    R32(PARAM_RESULT) = res;
    R32(PARAM_AUX) = p->aux ? p->aux : SRAM_SCRATCH;
    /* Optional concurrent DSP hammer: start Jerry looping over DRAM so the Tom
     * probe times against real cross-master bus contention. */
    if (p->ds) {
        copy16(D_RAM, p->ds, p->de);
        R32(D_PC) = D_RAM;
        R32(D_CTRL) = 1;
    }
    if (p->op)
        op_display(1); /* OP scans DRAM every displayed line */
    R32(G_PC) = G_RAM;
    R32(G_CTRL) = 1;

    if (mode == 0) {
        guard = 60000000UL; /* busy-poll: the intended mode-A bus noise */
        while (R32(res + 12) != MAGIC_DONE && --guard)
            ;
    } else {
        guard = 2000; /* fields (~33 s): one DRAM read per VI wake */
        while (R32(res + 12) != MAGIC_DONE && --guard)
            cpu_stop();
    }
    if (p->ds)
        R32(D_CTRL) = 0; /* stop the DSP hammer */
    if (p->op)
        op_display(0);
    return guard != 0;
}

static void report(int idx, int mode, int ok)
{
    const struct probe *p = &probes[idx];
    u32 res = slot_of(idx, mode);
    char *d = line;
    d = put(d, "CAL ");
    d = put(d, p->name);
    d = put(d, mode ? " B " : " A ");
    if (!ok) {
        d = put(d, "TIMEOUT\n");
    } else {
        d = put(d, "s=");
        d = puth(d, R32(res));
        d = put(d, " e=");
        d = puth(d, R32(res + 4));
        d = put(d, " w=");
        d = puth(d, R32(res + 8));
        d = put(d, " r=");
        d = puth(d, p->reps);
        d = put(d, "\n");
    }
    *d = 0;
    say(line);
}

void cal_main(void)
{
    int i, ok;
    u32 a;

#ifdef USE_SKUNK_CONSOLE
    skunk_init();
#endif
    say("Cobweb jsim calibration suite v1\n");

    R32(HDR) = 0x43414C42UL; /* 'CALB' */
    R32(HDR + 4) = 1;        /* table version */
    R32(HDR + 8) = (u32)NPROBES;
    R32(HDR + 12) = 0;       /* suite-done magic, set at the end */

    /* Seed the DRAM read buffer. */
    for (a = DRAM_BUF; a < DRAM_BUF + 0x40000UL; a += 4)
        R32(a) = a;

    /* Vertical interrupt for the stopped-68k mode. */
    R16(VI_REG) = 507;
    R16(INT1) = 1;
    irq_on();

    R32(0x001B0000UL) = 0; /* clear the DSP-hammer witness before the suite */
    /* Park the OP on a bare STOP so every baseline probe runs against a known,
     * minimal scan-out load; only lddramop swaps in the full-screen bitmap.
     * (OLP only — VMODE and the VI are left exactly as booted.) */
    op_display(0);

    for (i = 0; i < NPROBES; i++) {
        if (probes[i].bonly)
            continue; /* would starve the 68k's busy-poll — mode B only */
        ok = run_probe(i, 0);
        report(i, 0, ok);
        if (!ok)
            goto wedged;
    }
    for (i = 1; i < NPROBES; i++) { /* vcmod runs once; skip in mode B */
        ok = run_probe(i, 1);
        report(i, 1, ok);
        if (!ok)
            goto wedged;
    }

    {   /* did the DSP hammer actually run? ($D50D50D5 = yes) */
        char *d = line;
        d = put(d, "CAL DSPMARK val=");
        d = puth(d, R32(0x001B0000UL));
        d = put(d, "\n");
        *d = 0;
        say(line);
    }
    R32(HDR + 12) = MAGIC_DONE;
    say("CAL DONE\n");
#ifdef USE_SKUNK_CONSOLE
    skunk_close();
#endif
    for (;;)
        ;

wedged:
    say("CAL WEDGED: GPU stuck (bug 23 - no external GO clear). Power-cycle.\n");
    for (;;)
        ;
}
