// SPDX-License-Identifier: GPL-2.0-or-later
/* The device nodes: open, close, poll, mmap, and handing an ioctl to the code for its kind. */
#include <linux/ioctl.h>
#include <linux/mm.h>
#include <linux/mman.h>
#include <linux/poll.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/uaccess.h>
#include "nvfwd_guest.h"

#define NV_MAJOR 195

static unsigned long poll_ns = 20000;
module_param(poll_ns, ulong, 0644);

static long nvf_ioctl(struct file *file, unsigned int cmd, unsigned long arg)
{
	struct nvf_file *nf = file->private_data;

	if (_IOC_TYPE(cmd) == 'S')
		return show_ioctl(cmd, arg);

	if (nf->kind == NVFWD_DEV_UVM)
		return uvm_ioctl(nf, cmd, arg);
	return rm_ioctl(nf, cmd, arg);
}

static int nvf_release(struct inode *inode, struct file *file);

static int nvf_open_kind(struct file *file, int kind)
{
	struct nvf_file *nf = kzalloc(sizeof(*nf), GFP_KERNEL);
	struct nvfwd_req *r;
	int slot, rc;

	if (!nf)
		return -ENOMEM;
	slot = slot_get();
	r = slot_req(slot);
	r->op = NVFWD_OP_OPEN;
	r->a0 = kind;
	r->a1 = !!(file->f_flags & O_NONBLOCK);
	kick(slot);
	rc = r->ret;
	slot_put(slot);
	if (rc < 0) {
		kfree(nf);
		return rc;
	}
	nf->fdid = rc;
	nf->kind = kind;
	file->private_data = nf;
	if (kind == NVFWD_DEV_UVM) {
		nf->uvm_process = uvm_process_join();
		if (nf->uvm_process < 0) {
			rc = nf->uvm_process;
			nf->kind = NVFWD_DEV_CTL;
			nvf_release(NULL, file);
			return rc;
		}
	}
	return 0;
}

#ifndef NV_VERSION
#define NV_VERSION "unknown"
#endif

#define KMS_ALLOC_DEVICE 0
#define KMS_FREE_DEVICE 1
#define KMS_REPLY_AT 624 /* where the reply starts in NvKmsAllocDeviceParams */

/*
 * NvKmsAllocDeviceParams for each driver branch, from its nvkms-api.h. Branch 620 took subDeviceMask
 * out of the reply, so the parameters are 8 bytes shorter and the fields after it moved up.
 */
struct kms_branch {
	const char *version_start;
	u32 params_bytes;
	u32 reply[5]; /* the reply's first words: status, device handle, then what the branch has */
};
static const struct kms_branch kms_branches[] = {
	{ "615.", 1448, { 0 /* success */, 1 /* handle */, 1 /* subDeviceMask */, 0 /* heads */, 0 /* disps */ } },
	{ "620.", 1440, { 0 /* success */, 1 /* handle */, 0 /* heads */, 0 /* disps */, 0 } },
};

static const struct kms_branch *kms_branch(void)
{
	size_t i;

	for (i = 0; i < ARRAY_SIZE(kms_branches); i++)
		if (!strncmp(NV_VERSION, kms_branches[i].version_start, strlen(kms_branches[i].version_start)))
			return &kms_branches[i];
	return NULL;
}

/* /dev/nvidia-modeset never reaches the host. It answers for a device with no displays, and logs the rest. */
static long kms_ioctl(struct file *file, unsigned int cmd, unsigned long arg)
{
	const struct kms_branch *branch = kms_branch();
	struct { u32 cmd, size; u64 address; } p;

	if (copy_from_user(&p, (void __user *)arg, sizeof(p)))
		return -EFAULT;
	pr_info("nvfwd: modeset ioctl %x size %x\n", p.cmd, p.size);
	if (branch && p.cmd == KMS_ALLOC_DEVICE && p.size == branch->params_bytes) {
		static u8 zero[1448 - KMS_REPLY_AT];
		void __user *reply = u64_to_user_ptr(p.address + KMS_REPLY_AT);

		if (copy_to_user(reply, zero, branch->params_bytes - KMS_REPLY_AT) ||
		    copy_to_user(reply, branch->reply, sizeof(branch->reply)))
			return -EFAULT;
		return 0;
	}
	if (p.cmd == KMS_FREE_DEVICE)
		return 0;
	return -ENOTTY;
}

static const struct file_operations kms_fops = {
	.owner = THIS_MODULE,
	.unlocked_ioctl = kms_ioctl,
};

static int nvf_open(struct inode *inode, struct file *file)
{
	if (imajor(inode) == NV_MAJOR && iminor(inode) == 254) {
		file->f_op = &kms_fops;
		return 0;
	}
	return nvf_open_kind(file, imajor(inode) == nv.uvm_major ? NVFWD_DEV_UVM :
			     iminor(inode) == 255 ? NVFWD_DEV_CTL : NVFWD_DEV_GPU);
}

static int nvf_release(struct inode *inode, struct file *file)
{
	struct nvf_file *nf = file->private_data;
	int slot = slot_get();
	struct nvfwd_req *r = slot_req(slot);

	r->op = NVFWD_OP_CLOSE;
	r->fdid = nf->fdid;
	kick(slot);
	slot_put(slot);
	if (nf->kind == NVFWD_DEV_UVM)
		uvm_process_leave(nf->uvm_process);
	kfree(nf);
	return 0;
}

enum hrtimer_restart poll_tick(struct hrtimer *t)
{
	wake_up_all(&nv.pollq);
	return HRTIMER_NORESTART;
}

static __poll_t nvf_poll(struct file *file, poll_table *pt)
{
	struct nvf_file *nf = file->private_data;
	int slot, rc;
	struct nvfwd_req *r;

	poll_wait(file, &nv.pollq, pt);
	slot = slot_get();
	r = slot_req(slot);
	r->op = NVFWD_OP_POLL;
	r->fdid = nf->fdid;
	r->a0 = POLLIN | POLLPRI | POLLOUT;
	kick(slot);
	rc = r->ret;
	slot_put(slot);
	if (rc <= 0)
		hrtimer_start(&nv.poll_timer, ns_to_ktime(poll_ns), HRTIMER_MODE_REL);
	return rc > 0 ? (__force __poll_t)rc : 0;
}

/* ---- mmap ---- */

static void map_open(struct vm_area_struct *vma)
{
	struct nvf_map *m = vma->vm_private_data;

	refcount_inc(&m->refs);
}

static void map_close(struct vm_area_struct *vma)
{
	struct nvf_map *m = vma->vm_private_data;
	struct nvfwd_req *r;
	int slot;

	if (!refcount_dec_and_test(&m->refs))
		return;
	slot = slot_get();
	r = slot_req(slot);
	r->op = NVFWD_OP_MUNMAP;
	r->a0 = m->off;
	r->a1 = m->len;
	r->a2 = m->uvm;
	r->a3 = m->addr;
	kick(slot);
	slot_put(slot);
	kfree(m);
}

static const struct vm_operations_struct map_ops = {
	.open = map_open,
	.close = map_close,
};

static int nvf_mmap(struct file *file, struct vm_area_struct *vma)
{
	struct nvf_file *nf = file->private_data;
	struct nvf_map *m;
	struct nvfwd_req *r;
	u64 len = vma->vm_end - vma->vm_start, off;
	u32 caching = (nf->map_flags >> 23) & 7;
	int slot, rc;
	pgprot_t prot;

	m = kmalloc(sizeof(*m), GFP_KERNEL);
	if (!m)
		return -ENOMEM;
	slot = slot_get();
	r = slot_req(slot);
	r->op = NVFWD_OP_MMAP;
	r->fdid = nf->fdid;
	r->a0 = len;
	r->a1 = ((vma->vm_flags & VM_READ) ? PROT_READ : 0) | ((vma->vm_flags & VM_WRITE) ? PROT_WRITE : 0);
	r->a2 = vma->vm_pgoff;
	r->a3 = nf->kind == NVFWD_DEV_UVM;
	kick(slot);
	rc = r->ret;
	off = r->a3;
	slot_put(slot);
	if (rc) {
		kfree(m);
		return rc;
	}
	refcount_set(&m->refs, 1);
	m->off = off;
	m->len = len;
	m->uvm = nf->kind == NVFWD_DEV_UVM;
	vma->vm_private_data = m;
	m->addr = vma->vm_start;
	vma->vm_ops = &map_ops;
	if (m->uvm) {
		vm_flags_set(vma, VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP | VM_DONTCOPY);
		return remap_pfn_range(vma, vma->vm_start, (nv.win_base + off) >> PAGE_SHIFT, len, vma->vm_page_prot);
	}
	vm_flags_set(vma, VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP);
	/* 0 cached, 1 uncached, 2 write combined, 5 write back, 6 default, 7 uncached weak */
	if (caching == 2)
		prot = pgprot_writecombine(vma->vm_page_prot);
	else if (caching == 0 || caching == 5 || (caching == 6 && nf->kind == NVFWD_DEV_CTL))
		prot = vma->vm_page_prot;
	else
		prot = pgprot_noncached(vma->vm_page_prot);
	pr_info("nvfwd: mmap kind %d len %llx caching %u flags %x\n", nf->kind, len, caching, nf->map_flags);
	return remap_pfn_range(vma, vma->vm_start, (nv.win_base + off) >> PAGE_SHIFT, len, prot);
}

const struct file_operations nvf_fops = {
	.owner = THIS_MODULE,
	.open = nvf_open,
	.release = nvf_release,
	.unlocked_ioctl = nvf_ioctl,
	.poll = nvf_poll,
	.mmap = nvf_mmap,
};
