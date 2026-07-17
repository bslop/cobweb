//__BIGPEMU_SCRIPT_MODULE__
//__BIGPEMU_META_DESC__   "Dump Cybermorph DRAM framebuffer region ($100000-$1C0000) at a fixed frame for the seed emulator text comparison."
//__BIGPEMU_META_AUTHOR__ "the seed emulator"

#include "bigpcrt.h"

#define DUMP_FRAME   300u
#define FB_ADDR      0x100000u
#define FB_SIZE      0x0C0000u   /* 768 KB — covers the display + work buffers */
#define BUFSZ        4096u

static int      sEh = -1;
static int      sDone = 0;
static uint32_t sFrames = 0;
static uint8_t  sBuf[BUFSZ];

static void do_dump(void)
{
    uint64_t fh = fs_open_user("fbprobe.bin", 1);
    uint32_t off = 0, n;
    if (!fh)
        return;
    while (off < FB_SIZE)
    {
        n = FB_SIZE - off;
        if (n > BUFSZ) n = BUFSZ;
        bigpemu_jag_sysmemread(sBuf, FB_ADDR + off, n);
        fs_write(sBuf, n, fh);
        off += n;
    }
    fs_close(fh);
}

static uint32_t on_emu_frame(const int eh, void *pd)
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
    sEh = bigpemu_register_event_emu_thread_frame(pMod, on_emu_frame);
}

void bigp_shutdown()
{
    void *pMod = bigpemu_get_module_handle();
    if (sEh >= 0)
    {
        bigpemu_unregister_event(pMod, sEh);
        sEh = -1;
    }
}
