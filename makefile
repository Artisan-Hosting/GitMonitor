# Variables
APP_NAME = git_monitor
SERVICE_FILE = /etc/systemd/system/$(APP_NAME).service
BIN_DIR = /usr/local/bin
CONFIG_DIR = /etc/$(APP_NAME)

# Build binaries
build:
	cargo build --release

# Install binaries and config files
install: build
	@echo "Installing binaries..."
	install -m 0755 target/release/$(APP_NAME) $(BIN_DIR)
	install -m 0755 target/release/cli_credential $(BIN_DIR)

	@echo "Installing configuration files..."
	install -d $(CONFIG_DIR)
	install -m 0644 Config.toml Overrides.toml $(CONFIG_DIR)

	@echo "Installing systemd service file..."
	install -m 0644 $(APP_NAME).service $(SERVICE_FILE)
	systemctl daemon-reload
	systemctl enable $(APP_NAME)
	systemctl start $(APP_NAME)

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
