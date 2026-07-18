/* hwq_main.c — Cobweb hardware-question harness (68000 side).
 *
 * Launches the GPU probes in hwq_probes.s and reports raw results over the
 * Skunkboard console (jcp -c) and to a DRAM table at 0x100000. All
 * interpretation is trivial and done here (compare against GOOD/SENT), plus
 * printed raw for the host parser.
 */

typedef unsigned long u32;
typedef unsigned short u16;
#define R16(a) (*(volatile u16 *)(a))
#define R32(a) (*(volatile u32 *)(a))

#define G_RAM 0xF03000UL
#define G_PC 0xF02110UL
#define G_CTRL 0xF02114UL
#define PARAM_RES (G_RAM + 0xF84)
#define RES 0x100000UL      /* result table */
#define GOODADDR 0x140000UL /* where the probe's load reads from */
#define GOODVAL 0x600D600DUL
#define SENTVAL 0xBADBAD00UL
#define MAGIC 0xC0DED04EUL

extern void cpu_stop(void);
extern void irq_on(void);
#ifdef USE_SKUNK_CONSOLE
extern void skunk_init(void);
extern void skunk_puts(const char *s);
extern void skunk_close(void);
#endif

extern char p_xjump_s[], p_xjump_e[];
extern char p_ctrl_s[], p_ctrl_e[];
extern char p_vcfull_s[], p_vcfull_e[];

static char line[96];

static void copy16(u32 dst, const char *s, const char *e)
{
    volatile u16 *d = (volatile u16 *)dst;
    const u16 *p = (const u16 *)s;
    while ((const char *)p < e)
        *d++ = *p++;
}

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

static int run_probe(const char *ks, const char *ke, u32 res)
{
    u32 guard = 60000000UL;
    R32(res + 12) = 0;
    copy16(G_RAM, ks, ke);
    R32(PARAM_RES) = res;
    R32(G_PC) = G_RAM;
    R32(G_CTRL) = 1;
    while (R32(res + 12) != MAGIC && --guard)
        ;
    return guard != 0;
}

static void report(const char *tag, u32 res)
{
    char *d = line;
    u32 v = R32(res);
    d = put(d, "HWQ ");
    d = put(d, tag);
    d = put(d, " val=");
    d = puth(d, v);
    if (v == GOODVAL)
        d = put(d, "  [GOOD: scoreboard held]");
    else if (v == SENTVAL)
        d = put(d, "  [SENTINEL: STALE read]");
    d = put(d, "\n");
    *d = 0;
    say(line);
}

void cal_main(void)
{
#ifdef USE_SKUNK_CONSOLE
    skunk_init();
#endif
    say("Cobweb hardware-question probes v1\n");

    R32(GOODADDR) = GOODVAL; /* seed the external-load value */
    R16(0xF0004E) = 0xFFFF;  /* suppress VI */
    irq_on();

    if (!run_probe(p_xjump_s, p_xjump_e, RES)) {
        say("HWQ XJUMP  TIMEOUT\n");
        goto done;
    }
    /* Q1a: load consumed across a taken jump */
    report("XJUMP ", RES);

    if (!run_probe(p_ctrl_s, p_ctrl_e, RES + 16)) {
        say("HWQ CTRL   TIMEOUT\n");
        goto done;
    }
    /* Q1b: control — straight-line load+consume (must be GOOD) */
    report("CTRL  ", RES + 16);

    if (!run_probe(p_vcfull_s, p_vcfull_e, RES + 32)) {
        say("HWQ VCFULL TIMEOUT\n");
        goto done;
    }
    /* Q2: unmasked VC max — hex; modulus = max+1 (523/$20B => 524;
     * ~2570/$A0A => 2571). Host parser converts. */
    {
        char *d = line;
        d = put(d, "HWQ VCFULL max=");
        d = puth(d, R32(RES + 32));
        d = put(d, " (modulus = max+1)\n");
        *d = 0;
        say(line);
    }

    say("HWQ DONE\n");
done:
#ifdef USE_SKUNK_CONSOLE
    skunk_close();
#endif
    for (;;)
        ;
}
