# Cobweb jsim calibration ROMs.
#
# Two builds from the same source:
#   calib_skunk.cof — Skunkboard bench build: results over the USB console.
#   calib_sim.cof   — silent build for jagemu / GameDrive: results in DRAM
#                     only (read with `jagemu peek --at 0x100000`).
#
# Toolchain: cubanismo/jaguar-sdk Docker image (rmac / rln / m68k-aout-gcc),
# the same as the porting corpus.

BUILD := build

DOCKER_RUN := docker run --rm -v "$(CURDIR)":/work -w /work \
              -e PATH=/jaguar-sdk/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
              cubanismo/jaguar-sdk

RMAC := $(DOCKER_RUN) rmac
RLN  := $(DOCKER_RUN) rln
CC68 := $(DOCKER_RUN) m68k-aout-gcc

SKUNKINC := /jaguar-sdk/jaguar/skunk/include
SKUNKLIB := /jaguar-sdk/jaguar/skunk/lib

CFLAGS := -m68000 -ffreestanding -nostdlib -O2 -Wall

COMMON := $(BUILD)/bootstub.o $(BUILD)/probes.o
LAST   := $(BUILD)/lastobj.o

.PHONY: all clean sim
all: $(BUILD)/calib_skunk.cof $(BUILD)/calib_sim.cof

sim: $(BUILD)/calib_sim.cof

$(BUILD):
	mkdir -p $(BUILD)

$(BUILD)/%.o: %.s | $(BUILD)
	$(RMAC) -fb -i. -o $@ $<

$(BUILD)/skunk.o: skunk.s | $(BUILD)
	$(RMAC) -fb -i. -o $@ $<

$(BUILD)/skunkglue.o: skunkglue.s | $(BUILD)
	$(RMAC) -fb -i. -o $@ $<

$(BUILD)/main_skunk.o: main.c | $(BUILD)
	$(CC68) $(CFLAGS) -DUSE_SKUNK_CONSOLE -c -o $@ $<

$(BUILD)/main_sim.o: main.c | $(BUILD)
	$(CC68) $(CFLAGS) -c -o $@ $<

$(BUILD)/calib_skunk.cof: $(COMMON) $(BUILD)/main_skunk.o $(BUILD)/skunkglue.o $(BUILD)/skunk.o $(LAST)
	$(RLN) -e -rq -a 4000 x x -o $@ $(BUILD)/bootstub.o $(BUILD)/main_skunk.o $(BUILD)/probes.o $(BUILD)/skunkglue.o $(BUILD)/skunk.o $(LAST)

$(BUILD)/calib_sim.cof: $(COMMON) $(BUILD)/main_sim.o $(LAST)
	$(RLN) -e -rq -a 4000 x x -o $@ $(BUILD)/bootstub.o $(BUILD)/main_sim.o $(BUILD)/probes.o $(LAST)

clean:
	rm -rf $(BUILD)
