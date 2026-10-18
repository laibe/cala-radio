#!/usr/bin/env nu

# Manage the systemd unit that keeps the radio running.
#
# Deliberately separate from build.nu: building and deploying a binary is a
# routine thing to do, installing a system service is not. build.nu never
# touches the service.
#
#   nu supervisor.nu install     # copy the unit to the Pi, enable + start it
#   nu supervisor.nu status      # is it running, and since when
#   nu supervisor.nu logs        # recent output
#   nu supervisor.nu logs --follow
#   nu supervisor.nu restart
#   nu supervisor.nu uninstall
#
# Deploy a new binary with `nu build.nu --release --deploy`, then
# `nu supervisor.nu restart`.

const HOST = "cala-radio"
const UNIT = "radio.service"
const REMOTE_UNIT = "/etc/systemd/system/radio.service"

def main [] {
    print "Manage the radio systemd service. Subcommands:"
    print ""
    print "  install     copy the unit file to the Pi, enable and start it"
    print "  status      show whether the service is running"
    print "  logs        show recent journal output (--follow to tail)"
    print "  restart     restart the service (after deploying a new binary)"
    print "  uninstall   stop, disable, and remove the unit"
    print ""
    print $"Target host: ($HOST)  \(override with --host\)"
}

# Install the unit file and enable the service at boot.
def "main install" [
    --host: string = $HOST  # ssh host alias for the Pi
] {
    if not ($UNIT | path exists) {
        error make { msg: $"($UNIT) not found — run this from the repo root" }
    }

    print $"Copying ($UNIT) to ($host)..."
    # Land it in /tmp first; the remote user needs sudo to write into
    # /etc/systemd/system.
    rsync -avz $UNIT $"($host):/tmp/($UNIT)"

    print "Installing and enabling..."
    # `-t` allocates a TTY: without one, sudo has nowhere to prompt for a
    # password and fails with "a terminal is required".
    ssh -t $host $"sudo mv /tmp/($UNIT) ($REMOTE_UNIT) && sudo chown root:root ($REMOTE_UNIT) && sudo systemctl daemon-reload && sudo systemctl enable --now radio.service"

    print ""
    print "Installed. Useful checks:"
    print "  nu supervisor.nu status"
    print "  nu supervisor.nu logs"
    print ""
    print "The service restarts on failure and is watchdog-supervised, so a"
    print "hang is caught as well as a crash."
}

# Show service state.
def "main status" [--host: string = $HOST] {
    ssh $host "systemctl status radio.service --no-pager --lines=0"
}

# Show the journal for the service.
def "main logs" [
    --host: string = $HOST
    --follow (-f)          # tail the log instead of printing recent lines
    --lines: int = 100     # how many lines of history
] {
    if $follow {
        ssh -t $host $"journalctl -u radio.service -n ($lines) -f"
    } else {
        ssh $host $"journalctl -u radio.service -n ($lines) --no-pager"
    }
}

# Restart the service, e.g. after deploying a new binary.
def "main restart" [--host: string = $HOST] {
    ssh -t $host "sudo systemctl restart radio.service"
    print "Restarted."
    main status --host $host
}

# Stop and remove the service.
def "main uninstall" [--host: string = $HOST] {
    ssh -t $host $"sudo systemctl disable --now radio.service; sudo rm -f ($REMOTE_UNIT); sudo systemctl daemon-reload"
    print "Uninstalled."
}
