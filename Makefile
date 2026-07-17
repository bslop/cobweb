# Cobweb — one command to anything. `make help` lists everything.

.PHONY: help sim test run calib bench clean

help:
	@echo "Cobweb quick targets:"
	@echo "  make sim              build the emulator/simulator (needs Rust)"
	@echo "  make test             run the full test suite"
	@echo "  make run ROM=x.cof    boot a ROM 60 frames, print machine state JSON"
	@echo "  make shot ROM=x.cof   boot a ROM, save true-OP screenshot to shot.png"
	@echo "  make calib            build calibration ROMs (needs Docker; pulls"
	@echo "                        cubanismo/jaguar-sdk on first use)"
	@echo "  make bench            run the calibration suite in the simulator and"
	@echo "                        print the timing table (no hardware needed)"
	@echo "  make compat JAGUAR_ROMS=/path/to/carts"
	@echo "                        sweep every cart image, regenerate the"
	@echo "                        compatibility report data"
	@echo ""
	@echo "New here? Read docs/quickstart.md (humans) or AGENTS.md (AI agents)."

sim:
	cargo build --release --manifest-path sim/Cargo.toml
	@echo "built: sim/target/release/jagemu"

test:
	cargo test --manifest-path sim/Cargo.toml

run: sim
	sim/target/release/jagemu run $(ROM) --frames $(or $(FRAMES),60) --fidelity $(or $(FIDELITY),silicon)

shot: sim
	sim/target/release/jagemu screenshot $(ROM) --frames $(or $(FRAMES),120) -o shot.png
	@echo "wrote shot.png (true Object Processor scan-out)"

calib:
	$(MAKE) -C calib

bench: sim
	@test -f calib/build/calib_sim.cof || { echo "calibration ROM missing — run 'make calib' first (needs Docker)"; exit 1; }
	sim/target/release/jagemu peek calib/build/calib_sim.cof --at 0x100000 --len 1024 \
		--frames 3000 --fidelity silicon > /tmp/cobweb_bench.json
	python3 calib/parse_results.py --peek /tmp/cobweb_bench.json

compat: sim
	@test -n "$(JAGUAR_ROMS)" || { echo "set JAGUAR_ROMS=/path/to/cart/images"; exit 1; }
	bash bench/compat_sweep.sh "$(JAGUAR_ROMS)"

clean:
	cargo clean --manifest-path sim/Cargo.toml
	$(MAKE) -C calib clean
