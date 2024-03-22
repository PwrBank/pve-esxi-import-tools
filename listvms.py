#!/usr/bin/python3

from typing import List, Dict, Optional
import json
import os
import ssl
import sys
from pyVim.connect import SmartConnect, Disconnect
from pyVmomi import vim


def get_datacenter_of_vm(vm: vim.VirtualMachine) -> Optional[vim.Datacenter]:
    """Find the Datacenter object a VM belongs to."""
    current = vm.parent
    while current:
        if isinstance(current, vim.Datacenter):
            return current
        current = current.parent
    return None


def list_vms(service_instance: vim.ServiceInstance) -> List[vim.VirtualMachine]:
    """List all VMs on the ESXi/vCenter server."""
    content = service_instance.content
    vm_view = content.viewManager.CreateContainerView(
        content.rootFolder,
        [vim.VirtualMachine],
        True,
    )
    vms = vm_view.view
    vm_view.Destroy()
    return vms

def parse_file_path(path):
    """Parse a path of the form '[datastore] file/path'"""
    datastore_name, relative_path = path.split('] ', 1)
    datastore_name = datastore_name.strip('[')
    return (datastore_name, relative_path)

def get_vm_vmx_info(vm: vim.VirtualMachine) -> Dict[str, str]:
    """Extract VMX file path and checksum from a VM object."""
    datastore_name, relative_vmx_path = parse_file_path(vm.config.files.vmPathName)
    return {
        'datastore': datastore_name,
        'path': relative_vmx_path,
        'checksum': vm.config.vmxConfigChecksum.hex() if vm.config.vmxConfigChecksum else 'N/A'
    }

def get_vm_disk_info(vm: vim.VirtualMachine) -> Dict[str, int]:
    disks = []
    for device in vm.config.hardware.device:
        if type(device).__name__ == 'vim.vm.device.VirtualDisk':
            try:
                (datastore, path) = parse_file_path(device.backing.fileName)
                capacity = device.capacityInBytes
                disks.append({
                    'datastore': datastore,
                    'path': path,
                    'capacity': capacity,
                })
            except Exception as err:
                # if we can't figure out the disk stuff that's fine...
                print("failed to get disk information for esxi vm: ", err, file=sys.stderr)
    return disks

def get_all_datacenters(service_instance: vim.ServiceInstance) -> List[vim.Datacenter]:
    """Retrieve all datacenters from the ESXi/vCenter server."""
    content = service_instance.content
    dc_view = content.viewManager.CreateContainerView(content.rootFolder, [vim.Datacenter], True)
    datacenters = dc_view.view
    dc_view.Destroy()
    return datacenters

def main():
    if sys.argv[1] == '--skip-cert-verification':
        del sys.argv[1]
        ssl_context = ssl._create_unverified_context()
    else:
        ssl_context = None

    esxi_host = sys.argv[1]
    esxi_user = sys.argv[2]
    esxi_password_file = sys.argv[3]

    esxi_password = ''
    with open(esxi_password_file) as f:
        esxi_password = f.read()
        if esxi_password.endswith('\n'):
            esxi_password = esxi_password[:-1]

    try:
        si = SmartConnect(
            host=esxi_host,
            user=esxi_user,
            pwd=esxi_password,
            sslContext=ssl_context,
        )
    except ssl.SSLCertVerificationError as err:
        print("failed to verify certificate - add the CA of your ESXi to the system trust store or skip verification", file=sys.stderr)
        sys.exit(1)
    except Exception as err:
        print(f"failed to connect: {err}", file=sys.stderr)
        sys.exit(1)

    try:
        datacenters = get_all_datacenters(si)
        vms = list_vms(si)
        data = {}
        for vm in vms:
            name = 'vm ' + vm.name
            try:
                dc = get_datacenter_of_vm(vm)
                vm_info = {
                    'config': get_vm_vmx_info(vm),
                    'disks': get_vm_disk_info(vm),
                    'power': vm.runtime.powerState,
                }
                datastore_info = {ds.name: ds.url for ds in vm.config.datastoreUrl}
                data.setdefault(dc.name, {}).setdefault('vms', {})[vm.name] = vm_info
                data.setdefault(dc.name, {}).setdefault('datastores', {}).update(datastore_info)
            except Exception as err:
                print("failed to get info for", name, ':', err, file=sys.stderr)

        print(json.dumps(data, indent=2))
    finally:
        Disconnect(si)

if __name__ == "__main__":
    main()
