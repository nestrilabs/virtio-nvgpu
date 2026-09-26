// SPDX-License-Identifier: Apache-2.0
/*
 * nvgpu-poweroff -- end the VM from PID 1 without an init system.
 *
 * sync, then reboot(2) with RB_POWER_OFF (nesbox's ACPI sleep register ends
 * the VM) or, given "reboot", RB_AUTOBOOT (its reset register does). Nothing
 * else in the image can do this: util-linux has no poweroff, and there is no
 * systemd to ask.
 */
#include <stdio.h>
#include <string.h>
#include <sys/reboot.h>
#include <unistd.h>

int main(int argc, char **argv)
{
	int cmd = RB_POWER_OFF;

	if (argc > 1 && !strcmp(argv[1], "reboot"))
		cmd = RB_AUTOBOOT;
	sync();
	reboot(cmd);
	perror("nvgpu-poweroff: reboot");
	return 1;
}
