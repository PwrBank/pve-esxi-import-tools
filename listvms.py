#!/usr/bin/python3

from typing import List, Dict, Optional
import sys
import json
import os
from pyVim.connect import SmartConnectNoSSL, Disconnect
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


def get_vm_vmx_info(vm: vim.VirtualMachine) -> Dict[str, str]:
    """Extract VMX file path and checksum from a VM object."""
    vmx_path = vm.config.files.vmPathName
    datastore_name, relative_vmx_path = vmx_path.split('] ', 1)
    datastore_name = datastore_name.strip('[')
    return {
        'datastore': datastore_name,
        'path': relative_vmx_path,
        'checksum': vm.config.vmxConfigChecksum.hex() if vm.config.vmxConfigChecksum else 'N/A'
    }


def get_all_datacenters(service_instance: vim.ServiceInstance) -> List[vim.Datacenter]:
    """Retrieve all datacenters from the ESXi/vCenter server."""
    content = service_instance.content
    dc_view = content.viewManager.CreateContainerView(content.rootFolder, [vim.Datacenter], True)
    datacenters = dc_view.view
    dc_view.Destroy()
    return datacenters


def main():
    esxi_host = sys.argv[1]
    esxi_user = sys.argv[2]
    esxi_password_file = sys.argv[3]

    esxi_password = ''
    with open(esxi_password_file) as f:
        esxi_password = f.read()
        if esxi_password.endswith('\n'):
            esxi_password = esxi_password[:-1]

    try:
        si = SmartConnectNoSSL(host=esxi_host, user=esxi_user, pwd=esxi_password)
    except OSError as err:
        print(f"failed to connect: {err}")
        sys.exit(1)

    try:
        datacenters = get_all_datacenters(si)
        vms = list_vms(si)
        data = {}

        for dc in datacenters:
            dc_data = {'vms': {}, 'datastores': {}}
            for vm in vms:
                if get_datacenter_of_vm(vm) == dc:
                    vm_info = {'config': get_vm_vmx_info(vm)}
                    datastore_info = {ds.name: ds.url for ds in vm.config.datastoreUrl}
                    dc_data['vms'][vm.name] = vm_info
                    dc_data['datastores'].update(datastore_info)

            data[dc.name] = dc_data

        print(json.dumps(data, indent=2))
    finally:
        Disconnect(si)

if __name__ == "__main__":
    main()
