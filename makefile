# Variables
APP_NAME = ais_gitmon
SERVICE_FILE = /etc/systemd/system/$(APP_NAME).service
BIN_DIR = /opt/artisan/bin/
CONFIG_DIR = /opt/artisan/etc/$(APP_NAME)

# Build binaries
build:
	cargo build --release

# Install binaries and config files
install: build
	@echo "Installing binaries..."
	@ais stop ais_gitmon

	install -m 0755 target/release/$(APP_NAME) $(BIN_DIR)
	install -m 0755 target/release/cli_credential $(BIN_DIR)
	@ln -sv $(BIN_DIR)/cli_credential /usr/bin/gitcf

	@echo "Installing configuration files..."
	install -d $(CONFIG_DIR)
	@for f in Config.toml Overrides.toml; do \
		if [ ! -f "$$f" ]; then \
			echo "  $$f not present in source tree, skipping"; \
		elif [ -e "$(CONFIG_DIR)/$$f" ]; then \
			echo "  $(CONFIG_DIR)/$$f already exists, leaving it in place"; \
		else \
			install -m 0644 "$$f" "$(CONFIG_DIR)/$$f"; \
		fi; \
	done

	@ais start ais_gitmon 

# Install binaries and config files
install_safe: build
	@echo "Installing binaries..."

	install -m 0755 target/release/$(APP_NAME) $(BIN_DIR)
	install -m 0755 target/release/cli_credential $(BIN_DIR)
	@ln -sv $(BIN_DIR)/cli_credential /usr/bin/gitcf

	@echo "Installing configuration files..."
	install -d $(CONFIG_DIR)
	@for f in Config.toml Overrides.toml; do \
		if [ ! -f "$$f" ]; then \
			echo "  $$f not present in source tree, skipping"; \
		elif [ -e "$(CONFIG_DIR)/$$f" ]; then \
			echo "  $(CONFIG_DIR)/$$f already exists, leaving it in place"; \
		else \
			install -m 0644 "$$f" "$(CONFIG_DIR)/$$f"; \
		fi; \
	done

# Uninstall binaries, config files, and service
uninstall:
	@echo "Stopping and removing systemd service..."
	systemctl stop $(APP_NAME)
	systemctl disable $(APP_NAME)
	rm -f $(SERVICE_FILE)
	systemctl daemon-reload

	@echo "Removing binaries..."
	rm -f $(BIN_DIR)/$(APP_NAME) $(BIN_DIR)/cli_credential

	@echo "Removing configuration files..."
	rm -rf $(CONFIG_DIR)

# Clean build artifacts
clean:
	cargo clean

# Create the credential file 
credential:
	cd $(CONFIG_DIR) && cli_credential

.PHONY: build install uninstall clean credential
