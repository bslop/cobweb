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
extern char p_blit1_s[], p_blit1_e[];
extern char p_blit2_s[], p_blit2_e[];
extern char p_blit4_s[], p_blit4_e[];
extern char p_blittex1_s[], p_blittex1_e[];
extern char p_blittexq_s[], p_blittexq_e[];
extern char p_blitrmw_s[], p_blitrmw_e[];
extern char p_dens2_s[], p_dens2_e[];
extern char p_dens6_s[], p_dens6_e[];
extern char p_dens14_s[], p_dens14_e[];
extern char p_dens30_s[], p_dens30_e[];
extern char p_fib_s[], p_fib_e[];
extern char p_divext_s[], p_divext_e[];
extern char p_divoff_s[], p_divoff_e[];
extern char p_ldcunderb_s[], p_ldcunderb_e[];
extern char p_divlat_s[], p_divlat_e[];
extern char p_ldjump_s[], p_ldjump_e[];
extern char p_ldjumprn_s[], p_ldjumprn_e[];
extern char p_mmult_s[], p_mmult_e[];
extern char p_mm_nov_s[], p_mm_nov_e[];
extern char p_mm_w1_s[], p_mm_w1_e[];
extern char p_mm_w3_s[], p_mm_w3_e[];
extern char p_mm_w3s_s[], p_mm_w3s_e[];
extern char p_mm_mmhi_s[], p_mm_mmhi_e[];
extern char p_mm_mmlo_s[], p_mm_mmlo_e[];
extern char p_mm_mrd_s[], p_mm_mrd_e[];
extern char p_mm_mmovf_s[], p_mm_mmovf_e[];
extern char p_mm_mm2_s[], p_mm_mm2_e[];
extern char p_mm_mmrow_s[], p_mm_mmrow_e[];
extern char p_mm_mm2s_s[], p_mm_mm2s_e[];
extern char p_mmultw_s[], p_mmultw_e[];
extern char p_mmulta_s[], p_mmulta_e[];
extern char p_face_s[], p_face_e[];
extern char p_facenb_s[], p_facenb_e[];
extern char p_facebr_s[], p_facebr_e[];
extern char p_ovlap_s[], p_ovlap_e[];
extern char p_serial_s[], p_serial_e[];
extern char p_bcmdidle_s[], p_bcmdidle_e[];
extern char p_bcmdbusy_s[], p_bcmdbusy_e[];
extern char p_ldunderb_s[], p_ldunderb_e[];
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
    int cpubench;     /* 68k-side benchmark, not a GPU kernel (see bench68k):
                       * 1 = DRAM read stream (fetch + data both external)
                       * 2 = register-only dbra loop (fetch-only bus traffic)
                       * 3 = OpenLara's exact framebuffer-copy loop (the
                       *     measured critical path: move.b (aX)+,(aY)+) */
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
    { "blit1   ", p_blit1_s, p_blit1_e, 0, 0, 128, DRAM_BUF },
    { "blit2   ", p_blit2_s, p_blit2_e, 0, 0, 128, DRAM_BUF },
    { "blit4   ", p_blit4_s, p_blit4_e, 0, 0, 128, DRAM_BUF },
    { "blittex1", p_blittex1_s, p_blittex1_e, 0, 0, 128, DRAM_BUF },
    { "blittexq", p_blittexq_s, p_blittexq_e, 0, 0, 128, DRAM_BUF },
    { "blitrmw ", p_blitrmw_s, p_blitrmw_e, 0, 0, 128, DRAM_BUF },
    { "ldunderb", p_ldunderb_s, p_ldunderb_e, 0, 0, 128, DRAM_BUF },
    { "dens2   ", p_dens2_s, p_dens2_e, 0, 0, 256, DRAM_BUF },
    { "dens6   ", p_dens6_s, p_dens6_e, 0, 0, 256, DRAM_BUF },
    { "dens14  ", p_dens14_s, p_dens14_e, 0, 0, 128, DRAM_BUF },
    { "dens30  ", p_dens30_s, p_dens30_e, 0, 0, 128, DRAM_BUF },
    { "ldcunder", p_ldcunderb_s, p_ldcunderb_e, 0, 0, 128, DRAM_BUF },
    { "fib     ", p_fib_s, p_fib_e, 0, 0, 128, DRAM_BUF },
    { "divext  ", p_divext_s, p_divext_e, 0, 0, 128, DRAM_BUF },
    { "divoff  ", p_divoff_s, p_divoff_e, 0, 0, 128, DRAM_BUF },
    { "mmultw  ", p_mmultw_s, p_mmultw_e, 0, 0, 256, DRAM_BUF },
    { "mmulta  ", p_mmulta_s, p_mmulta_e, 0, 0, 256, DRAM_BUF },
    { "face    ", p_face_s, p_face_e, 0, 0, 128, DRAM_BUF },
    { "facenb  ", p_facenb_s, p_facenb_e, 0, 0, 128, DRAM_BUF },
    { "facebr  ", p_facebr_s, p_facebr_e, 0, 0, 128, DRAM_BUF },
    { "ovlap   ", p_ovlap_s, p_ovlap_e, 0, 0, 64, DRAM_BUF },
    { "serial  ", p_serial_s, p_serial_e, 0, 0, 64, DRAM_BUF },
    { "bcmdidle", p_bcmdidle_s, p_bcmdidle_e, 0, 0, 128, DRAM_BUF },
    { "bcmdbusy", p_bcmdbusy_s, p_bcmdbusy_e, 0, 0, 128, DRAM_BUF },
    { "lddramop", p_lddram_s, p_lddram_e, 0, 0, 512, DRAM_BUF, 0, 0, 1, 1 },
    /* 68k-side: the ONLY probe that times the 68000 itself rather than Tom.
     * jsim charges the 68k textbook instruction cycles against a free bus, and
     * feeds m68k_on_bus one way only (68k slows the GPU, never the reverse) —
     * but on silicon the 68000 is the LOWEST-priority master, below the OP,
     * the Blitter and both RISCs. Mode A = OP parked on a bare STOP, mode B =
     * OP scanning a full 320x240 bitmap. The A/B delta IS the 68k's scan-out
     * tax, which jsim currently models as exactly zero. */
    { "m68kbus ", 0, 0, 0, 0, 30, 0, 0, 0, 0, 0, 1 },
    /* Fetch-only companion to m68kbus. jsim's 68000 is 1.56x too fast on a
     * DRAM READ stream (fetch + data conflated); this register-only loop has
     * no data accesses, so its hw/sim ratio isolates the FETCH penalty. If it
     * comes back near 1.0x the wait belongs on data only and wip/m68k-bus-wait
     * is mergeable with a data-only charge; if near 1.56x the whole model is
     * wrong somewhere subtler. */
    { "m68kreg ", 0, 0, 0, 0, 30, 0, 0, 0, 0, 0, 2 },
    { "m68kcpy ", 0, 0, 0, 0, 30, 0, 0, 0, 0, 0, 3 },
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

#define VC_REG 0xF00006UL

/* Time a fixed 68k DRAM read stream against the VC half-line counter. No GPU
 * kernel is involved: this measures the 68000's own throughput, with the OP
 * either idle (mode A) or saturating scan-out (mode B). Same result-slot
 * format as the GPU probes, so report()/parse_results.py need no special case.
 *
 * Safe by construction: nothing here can wedge the console. The 68k never
 * waits on another master (no busy-poll, no cpu_stop), video timing and the VI
 * are untouched, and op_display only swaps OLP between a bitmap and a STOP. */
/* Time-bounded 68k throughput: run for a FIXED number of fields and count how
 * much work completed. Reported as blocks-of-16-longs in the `end` slot, so a
 * HIGHER number is faster and mode A / mode B / jsim are directly comparable.
 *
 * The first design was work-bounded (fixed reps, measure elapsed time) and that
 * was wrong: it sampled VC once per rep, so once a saturating OP stretched a rep
 * past one field the wrap count silently undercounted, the self-abort could
 * never fire, and mode B simply never reported inside a 150 s capture window —
 * twice. Fixing the bound rather than guessing how slow "slow" gets: this form
 * terminates in `p->reps` fields no matter how badly the 68000 is starved, and
 * VC is sampled every 16 longs so a wrap cannot hide between samples even at a
 * 50x slowdown. */
static void bench68k(int idx, int mode)
{
    const struct probe *p = &probes[idx];
    u32 res = slot_of(idx, mode);
    volatile u32 *q;
    u32 cur, prev, wraps = 0, blocks = 0, j;

    if (mode)
        op_display(1);
    prev = R16(VC_REG) & 0x7FF;
    while (wraps < p->reps) {
        if (p->cpubench == 3) {
            /* The three instructions holding ~64% of OpenLara's frame
             * (histogram 714ea70): move.b (a0)+,(a1)+ / cmpa.l / bne. One
             * unit = 64 bytes copied, DRAM_BUF -> DRAM_BUF+64K, exactly the
             * hot loop's shape so the hw/sim ratio applies to the REAL
             * critical path — the fetch/data probes measure other mixes and
             * their ratios (1.26x/1.51x) contradict the whole-program fps
             * unless this loop times differently. */
            __asm__ volatile(
                "movea.l %0,%%a0\n"
                "movea.l %1,%%a1\n"
                "lea 64(%%a0),%%a2\n"
                "1: move.b (%%a0)+,(%%a1)+\n"
                "cmpa.l %%a2,%%a0\n"
                "bne.s 1b\n"
                : : "r"(DRAM_BUF), "r"(DRAM_BUF + 0x10000UL)
                : "a0", "a1", "a2", "cc", "memory");
        } else if (p->cpubench == 2) {
            /* Register-only unit of work: 16 taken DBRAs, no data access.
             * Inline asm so -O2 cannot delete the "useless" loop. The 68000
             * still FETCHES every iteration from DRAM — that is the point:
             * the hw/sim ratio of this probe isolates the instruction-fetch
             * cost, while m68kbus (DRAM reads) conflates fetch + data. The
             * wip/m68k-bus-wait question is exactly which of the two carries
             * the measured 1.56x. */
            __asm__ volatile(
                "moveq #15,%%d0\n"
                "1: dbra %%d0,1b\n"
                : : : "d0", "cc");
        } else {
            q = (volatile u32 *)DRAM_BUF;
            for (j = 0; j < 16; j++)
                (void)*q++;
        }
        blocks++;
        cur = R16(VC_REG) & 0x7FF;
        if (cur < prev)
            wraps++;
        prev = cur;
    }
    if (mode)
        op_display(0);
    R32(res) = 0;
    R32(res + 4) = blocks; /* the measurement: work done in p->reps fields */
    R32(res + 8) = 0;
    R32(res + 12) = MAGIC_DONE;
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

#ifndef DIVLAT_ONLY
    for (i = 0; i < NPROBES; i++) {
#ifdef FACE_ONLY
        if (probes[i].ks != p_face_s && probes[i].ks != p_facenb_s && probes[i].ks != p_facebr_s)
            continue;
#endif
#ifdef OVLAP_ONLY
        if (probes[i].ks != p_ovlap_s && probes[i].ks != p_serial_s)
            continue;
#endif
#ifdef CPUBENCH_ONLY
        if (!probes[i].cpubench)
            continue; /* fast ROM: 68k bench only, completes inside one capture */
#endif
        if (probes[i].bonly)
            continue; /* would starve the 68k's busy-poll — mode B only */
        if (probes[i].cpubench) {
            bench68k(i, 0);
            ok = 1; /* self-timed: cannot wedge, and `ok` must not stay unset */
        } else {
            ok = run_probe(i, 0);
        }
        report(i, 0, ok);
        if (!ok)
            goto wedged;
    }
    for (i = 1; i < NPROBES; i++) { /* vcmod runs once; skip in mode B */
#ifdef FACE_ONLY
        if (probes[i].ks != p_face_s && probes[i].ks != p_facenb_s && probes[i].ks != p_facebr_s)
            continue;
#endif
#ifdef OVLAP_ONLY
        if (probes[i].ks != p_ovlap_s && probes[i].ks != p_serial_s)
            continue;
#endif
#ifdef CPUBENCH_ONLY
        if (!probes[i].cpubench)
            continue;
#endif
        if (probes[i].cpubench) {
            bench68k(i, 1);
            ok = 1; /* self-timed: cannot wedge, and `ok` must not stay unset */
        } else {
            ok = run_probe(i, 1);
        }
        report(i, 1, ok);
        if (!ok)
            goto wedged;
    }

#endif /* DIVLAT_ONLY */

    {   /* p_divlat: DIV readable-latency by quotient correctness (round 6.2).
         * Custom result layout: 16 readbacks at DIVRES[0..15], magic at [64].
         * Prints CAL DIVLAT k=NN v=XXXXXXXX for each K; host reads the first
         * K whose value == 00000055 as the true readable latency. */
        u32 DIVRES = 0x00102000UL;
        u32 guard = 60000000UL;
        int k;
        R32(DIVRES + 128) = 0;
        copy16(G_RAM, p_divlat_s, p_divlat_e);
        R32(PARAM_RESULT) = DIVRES;
        R32(G_PC) = G_RAM;
        R32(G_CTRL) = 1;
        while (R32(DIVRES + 128) != MAGIC_DONE && --guard)
            ;
        for (k = 0; k < 16; k++) {
            char *d = line;
            d = put(d, "CAL DIVLAT k=");
            d = puth(d, (u32)k);
            d = put(d, " sm=");
            d = puth(d, R32(DIVRES + (u32)k * 8));      /* small (want 00000055) */
            d = put(d, " lg=");
            d = puth(d, R32(DIVRES + (u32)k * 8 + 4));  /* large (want 2AAAAAA5) */
            d = put(d, "\n");
            *d = 0;
            say(line);
        }
    }

    {   /* p_mm bisection ladder: WHICH ingredient of MMULT wedges real Tom.
         * The full p_mmult wedges (bug 23) in two formulations; these four
         * minimal arms each self-stop from bank 0 and write magic at RES+12.
         * Launch them in order; the FIRST that never writes magic is the wedge
         * trigger — and the board is then dead (bug 23), so stop the ladder.
         * jsim runs all four clean (the wedge is silicon-only). Prints:
         *   CAL MMBIS <nm> v0=.. v1=..   (arm completed — value also decodes
         *                                 the operand layout: nov=A0, w1=4,
         *                                 w3/w3s=20)
         *   CAL MMBIS <nm> WEDGED        (arm hung the GPU) */
        u32 RES = 0x00105000UL;
        char *ks[11], *ke[11];
        const char *nm[11];
        int a;
        /* Order: single-mmult arms first (never wedge), then multi-mmult by
         * DESCENDING settle between consecutive mmults so the known wedger
         * (mm2, zero settle) runs LAST — mmrow (~13-instr gap) and mm2s
         * (8-nop gap) get to report before any wedge kills the session. */
        ks[0]  = p_mm_nov_s;   ke[0]  = p_mm_nov_e;   nm[0]  = "nov";
        ks[1]  = p_mm_w1_s;    ke[1]  = p_mm_w1_e;    nm[1]  = "w1";
        ks[2]  = p_mm_w3_s;    ke[2]  = p_mm_w3_e;    nm[2]  = "w3";
        ks[3]  = p_mm_w3s_s;   ke[3]  = p_mm_w3s_e;   nm[3]  = "w3s";
        ks[4]  = p_mm_mmhi_s;  ke[4]  = p_mm_mmhi_e;  nm[4]  = "mmhi";
        ks[5]  = p_mm_mmlo_s;  ke[5]  = p_mm_mmlo_e;  nm[5]  = "mmlo";
        ks[6]  = p_mm_mrd_s;   ke[6]  = p_mm_mrd_e;   nm[6]  = "mrd";
        ks[7]  = p_mm_mmovf_s; ke[7]  = p_mm_mmovf_e; nm[7]  = "mmovf";
        ks[8]  = p_mm_mmrow_s; ke[8]  = p_mm_mmrow_e; nm[8]  = "mmrow";
        ks[9]  = p_mm_mm2s_s;  ke[9]  = p_mm_mm2s_e;  nm[9]  = "mm2s";
        ks[10] = p_mm_mm2_s;   ke[10] = p_mm_mm2_e;   nm[10] = "mm2";
        for (a = 0; a < 11; a++) {
            u32 guard = 60000000UL;
            u32 slot = RES + (u32)a * 0x20; /* distinct slot per arm (sim peek) */
            char *d = line;
            R32(slot + 12) = 0;
            copy16(G_RAM, ks[a], ke[a]);
            R32(PARAM_RESULT) = slot;
            R32(G_PC) = G_RAM;
            R32(G_CTRL) = 1;
            while (R32(slot + 12) != MAGIC_DONE && --guard)
                ;
            R32(G_CTRL) = 0; /* force-stop (fails under bug 23 if wedged) */
            d = put(d, "CAL MMBIS ");
            d = put(d, nm[a]);
            if (guard == 0) {
                d = put(d, " WEDGED\n");
                *d = 0;
                say(line);
                break; /* board dead (bug 23) — stop the ladder here */
            }
            d = put(d, " v0=");
            d = puth(d, R32(slot));
            d = put(d, " v1=");
            d = puth(d, R32(slot + 4));
            d = put(d, "\n");
            *d = 0;
            say(line);
        }
    }

    {   /* p_mmult: MMULT operand layout / s16 / MAC on real Tom (Phase-0 gate,
         * COBWEB_REQ_mmult_silicon_probe.md). Runs FIRST (right after DIVLAT) so
         * its one line lands in the healthy post-bounce USB window — it is the
         * highest-value result and the finicky Skunkboard link tends to drop
         * partway through a session. Prints
         *   CAL MMULT o0=.. o1=.. o2=.. ovf=.. m1=.. m2=..
         * Expected if silicon == jsim: 20 140 C80 FFFE0000 20 20 (hex).
         * o0=654 (0x28E) instead of 32 => column/transpose layout; m2==2*m1 =>
         * MMULT accumulates rather than resets.
         * DISABLED by default (2026-07-23): this multi-MMULT probe WEDGES real
         * Tom (bug 23) and would end the session on a bus-held hang. The MMBIS
         * ladder above supersedes it. Define RUN_OLD_MMULT to re-enable. */
#ifdef RUN_OLD_MMULT
        u32 MMRES = 0x00104000UL;
        u32 guard = 60000000UL;
        char *d;
        /* Start beacon: disambiguates a real GPU wedge from a USB dropout. If
         * "CAL MMSTART" prints but "CAL MMULT" never does, the GPU hung the bus
         * (probe bug); if neither prints, the link dropped before this block. */
        say("CAL MMSTART\n");
        R32(MMRES + 32) = 0;
        copy16(G_RAM, p_mmult_s, p_mmult_e);
        R32(G_PC) = G_RAM;
        R32(G_CTRL) = 1;
        while (R32(MMRES + 32) != MAGIC_DONE && --guard)
            ;
        /* Force the GPU to halt from the 68k side regardless of how the probe
         * exited — if MMULT left it running it would hold the bus and hang the
         * console; this guarantees we can read and print the results. */
        R32(G_CTRL) = 0;
        d = line;
        d = put(d, "CAL MMULT o0=");
        d = puth(d, R32(MMRES));
        d = put(d, " o1=");
        d = puth(d, R32(MMRES + 4));
        d = put(d, " o2=");
        d = puth(d, R32(MMRES + 8));
        d = put(d, " ovf=");
        d = puth(d, R32(MMRES + 12));
        d = put(d, " m1=");
        d = puth(d, R32(MMRES + 16));
        d = put(d, " m2=");
        d = puth(d, R32(MMRES + 20));
        d = put(d, "\n");
        *d = 0;
        say(line);
#endif /* RUN_OLD_MMULT */
    }

    {   /* p_ldjump: load consumed across a taken jump (round 5.2). Prints
         * CAL LDJUMP dram=XXXXXXXX sram=XXXXXXXX. Correct == truths
         * (ABCD1234 / 5678DEF0); anything else = scoreboard dropped across
         * the jump = the erratum. jsim (Silicon) serves the truths (stalls). */
        u32 LJRES = 0x00103000UL;
        u32 guard = 60000000UL;
        char *d;
        R32(LJRES + 8) = 0;
        copy16(G_RAM, p_ldjump_s, p_ldjump_e);
        R32(PARAM_RESULT) = LJRES;
        R32(G_PC) = G_RAM;
        R32(G_CTRL) = 1;
        while (R32(LJRES + 8) != MAGIC_DONE && --guard)
            ;
        d = line;
        d = put(d, "CAL LDJUMP dram=");
        d = puth(d, R32(LJRES));
        d = put(d, " sram=");
        d = puth(d, R32(LJRES + 4));
        d = put(d, "\n");
        *d = 0;
        say(line);
    }

    {   /* p_ldjumprn: load consumed across an ABSOLUTE jump (rN) — the one edge
         * p_ldjump left untested (COBWEB_REQ_jumprn_load_scoreboard_probe.md).
         * Prints CAL LDJUMPRN dram=XXXXXXXX sram=XXXXXXXX. Correct == truths
         * (ABCD1234 / 5678DEF0) = silicon scoreboards across jump(rN) too;
         * stale/garbage = the erratum is real for the absolute-jump form. */
        u32 LJRES = 0x00103100UL;
        u32 guard = 60000000UL;
        char *d;
        R32(LJRES + 8) = 0;
        copy16(G_RAM, p_ldjumprn_s, p_ldjumprn_e);
        R32(PARAM_RESULT) = LJRES;
        R32(G_PC) = G_RAM;
        R32(G_CTRL) = 1;
        while (R32(LJRES + 8) != MAGIC_DONE && --guard)
            ;
        d = line;
        d = put(d, "CAL LDJUMPRN dram=");
        d = puth(d, R32(LJRES));
        d = put(d, " sram=");
        d = puth(d, R32(LJRES + 4));
        d = put(d, "\n");
        *d = 0;
        say(line);
    }

#ifndef DIVLAT_ONLY
    {   /* did the DSP hammer actually run? ($D50D50D5 = yes) */
        char *d = line;
        d = put(d, "CAL DSPMARK val=");
        d = puth(d, R32(0x001B0000UL));
        d = put(d, "\n");
        *d = 0;
        say(line);
    }
#endif /* DIVLAT_ONLY (DSPMARK) */
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
