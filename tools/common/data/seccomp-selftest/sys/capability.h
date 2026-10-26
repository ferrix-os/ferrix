/* The libcap calls seccomp_bpf.c makes, over capget and capset: Ferrix's
 * build of Linux's selftest links no libcap. Only the effective set's flags
 * are read and changed, as the test asks. */
#ifndef FERRIX_SHIM_CAPABILITY_H
#define FERRIX_SHIM_CAPABILITY_H
#include <linux/capability.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/syscall.h>

typedef struct { struct __user_cap_header_struct header; struct __user_cap_data_struct data[2]; } *cap_t;
typedef int cap_value_t;
typedef int cap_flag_value_t;
typedef enum { CAP_EFFECTIVE = 0, CAP_PERMITTED = 1, CAP_INHERITABLE = 2 } cap_flag_t;
#define CAP_CLEAR 0
#define CAP_SET 1

static inline cap_t cap_get_proc(void)
{
	cap_t cap = calloc(1, sizeof(*cap));
	if (!cap)
		return NULL;
	cap->header.version = _LINUX_CAPABILITY_VERSION_3;
	if (syscall(SYS_capget, &cap->header, cap->data)) {
		free(cap);
		return NULL;
	}
	return cap;
}

static inline int cap_get_flag(cap_t cap, cap_value_t value, cap_flag_t flag, cap_flag_value_t *out)
{
	unsigned int word = value / 32, bit = 1u << (value % 32);
	unsigned int held = flag == CAP_EFFECTIVE ? cap->data[word].effective
		: flag == CAP_PERMITTED ? cap->data[word].permitted : cap->data[word].inheritable;
	*out = !!(held & bit);
	return 0;
}

static inline int cap_set_flag(cap_t cap, cap_flag_t flag, int count, const cap_value_t *values, int set)
{
	for (int at = 0; at < count; at++) {
		unsigned int word = values[at] / 32, bit = 1u << (values[at] % 32);
		unsigned int *held = flag == CAP_EFFECTIVE ? &cap->data[word].effective
			: flag == CAP_PERMITTED ? &cap->data[word].permitted : &cap->data[word].inheritable;
		*held = set ? (*held | bit) : (*held & ~bit);
	}
	return 0;
}

static inline int cap_set_proc(cap_t cap)
{
	return syscall(SYS_capset, &cap->header, cap->data);
}

static inline int cap_free(void *cap)
{
	free(cap);
	return 0;
}
#endif
