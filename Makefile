
bump:
	@if [ "$(ver)x" = "x" ]; then \
		echo "USAGE: make bump ver=0.1.0-alpha.1"; \
		exit 1; \
	fi
	./scripts/bump.sh $(ver)
	cargo build


# Helpers for Unyt tests
# start_influx:
# 	influxd

# configure_influx:
# 	configure_influx

# use_influx:
# 	use_influx

# start_telegraf:
# 	start_telegraf

run_hc:
	hc s clean && echo "1234" | hc s --piped create && echo "1234" | RUST_LOG=warn hc s --piped -f 8888 run

test_spend:
	RUST_LOG=debug MIN_AGENTS=3 cargo run --package unyt -- --connection-string ws://localhost:8888 --agents 3  --behaviour initiate:1  --behaviour spend:2 --duration 30

test_smart_agreements:
	RUST_LOG=debug NUMBER_OF_LINKS_TO_PROCESS=2 cargo run --package unyt -- --connection-string ws://localhost:8888 --agents 3 --behaviour initiate:1 --behaviour smart_agreements:2 --duration 30

build-happ:
	cd unyt && yarn build:happ
	mkdir -p happs/unyt
	cp unyt/workdir/unyt.happ happs/unyt/unyt.happ

build: 
	cargo build -p unyt

nomad_run_spend: prep_jobs_spend deploy_spend

nomad_run_smart_agreements: prep_jobs_smart_agreements deploy_smart_agreements

prep_jobs_spend:
	./nomad/generate_jobs.sh custom_unyt_spend

prep_jobs_smart_agreements:
	./nomad/generate_jobs.sh custom_unyt_smart_agreements

deploy_spend:
	@if [ ! -f nomad/token ]; then echo "Error: nomad/token file not found"; exit 1; fi
	nomad job run -address=https://nomad-server-01.holochain.org:4646 \
		-token=$$(cat nomad/token) \
		-ca-cert=./nomad/server-ca-cert.pem \
		./nomad/jobs/custom_unyt_spend.nomad.hcl

deploy_smart_agreements:
	@if [ ! -f nomad/token ]; then echo "Error: nomad/token file not found"; exit 1; fi
	nomad job run -address=https://nomad-server-01.holochain.org:4646 \
		-token=$$(cat nomad/token) \
		-ca-cert=./nomad/server-ca-cert.pem \
		./nomad/jobs/custom_unyt_smart_agreements.nomad.hcl