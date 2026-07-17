//__BIGPEMU_SCRIPT_MODULE__
//__BIGPEMU_META_DESC__   "Oracle: dump 68k/GPU/DSP state + DRAM chunk checksums at frame N for the seed emulator parity diffing."
//__BIGPEMU_META_AUTHOR__ "the seed emulator"

#include "bigpcrt.h"

#define DUMP_FRAME   200
#define MAGIC        0x4A41474Fu   /* 'JAGO' */
#define DRAM_SIZE    0x200000u
#define NUM_CHUNKS   64u
#define CHUNK_SIZE   (DRAM_SIZE / NUM_CHUNKS)   /* 32 KB */
#define BUFSZ        4096u

static int      sEmuFrameEh = -1;
static int      sDone = 0;
static uint32_t sFrames = 0;
static uint8_t  sBuf[BUFSZ];

static void w32(uint64_t fh, uint32_t v)
{
    uint8_t b[4];
    b[0] = (uint8_t)(v >> 24);
    b[1] = (uint8_t)(v >> 16);
    b[2] = (uint8_t)(v >> 8);
    b[3] = (uint8_t)v;
    fs_write(b, 4, fh);
}

/* FNV-1a hash of a memory range (read in BUFSZ chunks). */
static uint32_t range_hash(uint32_t addr, uint32_t size)
{
    uint32_t hash = 0x811c9dc5u;
    uint32_t off = 0;
    uint32_t i, n;
    while (off < size)
    {
        n = size - off;
        if (n > BUFSZ) n = BUFSZ;
        bigpemu_jag_sysmemread(sBuf, addr + off, n);
        for (i = 0; i < n; i++)
        {
            hash ^= sBuf[i];
            hash *= 16777619u;
        }
        off += n;
    }
    return hash;
}

static uint32_t mem_long(uint32_t addr)
{
    bigpemu_jag_sysmemread(sBuf, addr, 4);
    return ((uint32_t)sBuf[0] << 24) | ((uint32_t)sBuf[1] << 16)
         | ((uint32_t)sBuf[2] << 8) | (uint32_t)sBuf[3];
}

static void do_dump(void)
{
    uint64_t fh;
    int i;

    fh = fs_open_user("oracle.bin", 1);
    if (!fh)
        return;

    w32(fh, MAGIC);
    w32(fh, (uint32_t)bigpemu_jag_get_frame_count());
    w32(fh, bigpemu_jag_get_line_count());

    /* 68000: PC, D0-D7, A0-A7 */
    w32(fh, bigpemu_jag_m68k_get_pc());
    for (i = 0; i < 8; i++) w32(fh, bigpemu_jag_m68k_get_dreg((uint32_t)i));
    for (i = 0; i < 8; i++) w32(fh, bigpemu_jag_m68k_get_areg((uint32_t)i));

    /* GPU: PC, CTRL, FLAGS, R0-R31 (current bank) */
    w32(fh, bigpemu_jag_gpu_get_pc());
    w32(fh, mem_long(0xF02114u));
    w32(fh, mem_long(0xF02100u));
    for (i = 0; i < 32; i++) w32(fh, bigpemu_jag_gpu_curbank_get_reg((uint32_t)i));

    /* DSP: PC, CTRL, FLAGS, R0-R31 */
    w32(fh, bigpemu_jag_dsp_get_pc());
    w32(fh, mem_long(0xF1A114u));
    w32(fh, mem_long(0xF1A100u));
    for (i = 0; i < 32; i++) w32(fh, bigpemu_jag_dsp_curbank_get_reg((uint32_t)i));

    /* DRAM divergence map: FNV hash of each 32 KB chunk of the 2 MB DRAM. */
    w32(fh, NUM_CHUNKS);
    for (i = 0; i < (int)NUM_CHUNKS; i++)
        w32(fh, range_hash((uint32_t)i * CHUNK_SIZE, CHUNK_SIZE));

    fs_close(fh);
}

static uint32_t on_emu_frame(const int eventHandle, void *pEventData)
{
    if (sDone)
        return 0;
    sFrames++;
    if (sFrames >= DUMP_FRAME)
    {
        sDone = 1;
        do_dump();
    }
    return 0;
}

void bigp_init()
{
    void *pMod = bigpemu_get_module_handle();
    sDone = 0;
    sFrames = 0;
    sEmuFrameEh = bigpemu_register_event_emu_thread_frame(pMod, on_emu_frame);
}

void bigp_shutdown()
{
    void *pMod = bigpemu_get_module_handle();
    if (sEmuFrameEh >= 0)
    {
        bigpemu_unregister_event(pMod, sEmuFrameEh);
        sEmuFrameEh = -1;
    }
}
