#!/usr/bin/python3

import argparse
import dataclasses
import json
import ssl
import sys
import textwrap

from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Generator

from pyVim.connect import SmartConnect, Disconnect
from pyVmomi import vim


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="listvms",
        description="List VMs on an ESXi host.",
    )

    parser.add_argument(
        "--skip-cert-verification",
        help="Skip the verification of TLS certs, e.g. to allow self-signed"
        " certs.",
        action="store_true",
    )

    parser.add_argument(
        "hostname",
        help="The name or address of the ESXi host.",
    )

    parser.add_argument(
        "username",
        help="The name of the user to connect with.",
    )

    parser.add_argument(
        "password_file",
        help="The file which contains the password for the provided username.",
        type=Path,
    )

    return parser.parse_args()


@dataclass
class EsxiConnectonArgs:
    hostname: str
    username: str
    password_file: Path
    skip_cert_verification: bool = False


@contextmanager
def connect_to_esxi_host(
    args: EsxiConnectonArgs,
) -> Generator[vim.ServiceInstance, None, None]:
    """Opens a connection to an ESXi host with the given username and password
    contained in the password file.
    """
    ssl_context = (
        ssl._create_unverified_context()
        if args.skip_cert_verification
        else None
    )

    with open(args.password_file) as pw_file:
        password = pw_file.read()
        if password.endswith("\n"):
            password = password[:-1]

    connection = None

    try:
        connection = SmartConnect(
            host=args.hostname,
            user=args.username,
            pwd=password,
            sslContext=ssl_context,
        )

        yield connection

    except ssl.SSLCertVerificationError:
        raise ConnectionError(
            "Failed to verify certificate - add the CA of your ESXi to the "
            "system trust store or skip verification",
        )

    finally:
        if connection is not None:
            Disconnect(connection)


@dataclass
class VmVmxInfo:
    datastore: str
    path: str
    checksum: str


@dataclass
class VmDiskInfo:
    datastore: str
    path: str
    capacity: int


@dataclass
class VmInfo:
    config: VmVmxInfo
    disks: list[VmDiskInfo]
    power: str


def json_dump_helper(obj: Any) -> Any:
    """Converts otherwise unserializable objects to types that can be
    serialized as JSON.

    Raises:
        TypeError: If the conversion of the object is not supported.
    """
    if dataclasses.is_dataclass(obj):
        return dataclasses.asdict(obj)

    raise TypeError(
        f"Can't make object of type {type(obj)} JSON-serializable: {repr(obj)}"
    )


def get_datacenter_of_vm(vm: vim.VirtualMachine) -> vim.Datacenter | None:
    """Find the Datacenter object a VM belongs to."""
    current = vm.parent
    while current:
        if isinstance(current, vim.Datacenter):
            return current
        current = current.parent
    return None


def list_vms(service_instance: vim.ServiceInstance) -> list[vim.VirtualMachine]:
    """List all VMs on the ESXi/vCenter server."""
    content = service_instance.content
    vm_view: Any = content.viewManager.CreateContainerView(
        content.rootFolder,
        [vim.VirtualMachine],
        True,
    )
    vms = vm_view.view
    vm_view.Destroy()
    return vms


def parse_file_path(path) -> tuple[str, str]:
    """Parse a path of the form '[datastore] file/path'"""
    datastore_name, relative_path = path.split("] ", 1)
    datastore_name = datastore_name.strip("[")
    return (datastore_name, relative_path)


def get_vm_vmx_info(vm: vim.VirtualMachine) -> VmVmxInfo:
    """Extract VMX file path and checksum from a VM object."""
    datastore_name, relative_vmx_path = parse_file_path(
        vm.config.files.vmPathName
    )

    return VmVmxInfo(
        datastore=datastore_name,
        path=relative_vmx_path,
        checksum=vm.config.vmxConfigChecksum.hex()
        if vm.config.vmxConfigChecksum
        else "N/A",
    )


def get_vm_disk_info(vm: vim.VirtualMachine) -> list[VmDiskInfo]:
    disks = []
    for device in vm.config.hardware.device:
        if isinstance(device, vim.vm.device.VirtualDisk):
            try:
                (datastore, path) = parse_file_path(device.backing.fileName)
                capacity = device.capacityInBytes
                disks.append(VmDiskInfo(datastore, path, capacity))
            except Exception as err:
                # if we can't figure out the disk stuff that's fine...
                print(
                    "failed to get disk information for esxi vm: ",
                    err,
                    file=sys.stderr,
                )
    return disks


def get_all_datacenters(
    service_instance: vim.ServiceInstance,
) -> list[vim.Datacenter]:
    """Retrieve all datacenters from the ESXi/vCenter server."""
    content = service_instance.content
    dc_view: Any = content.viewManager.CreateContainerView(
        content.rootFolder, [vim.Datacenter], True
    )
    datacenters = dc_view.view
    dc_view.Destroy()
    return datacenters


def fetch_and_update_vm_data(vm: vim.VirtualMachine, data: dict[Any, Any]):
    """Fetches all required VM, datastore and datacenter information, and
    then updates the given `dict`.

    Raises:
        RuntimeError: If looking up the datacenter for the given VM fails.
    """
    datacenter = get_datacenter_of_vm(vm)
    if datacenter is None:
        raise RuntimeError(f"Failed to lookup datacenter for VM {vm.name}")

    data.setdefault(datacenter.name, {})

    vms = data[datacenter.name].setdefault("vms", {})
    datastores = data[datacenter.name].setdefault("datastores", {})

    vms[vm.name] = VmInfo(
        config=get_vm_vmx_info(vm),
        disks=get_vm_disk_info(vm),
        power=str(vm.runtime.powerState),
    )

    datastores.update({ds.name: ds.url for ds in vm.config.datastoreUrl})


def main():
    args = parse_args()

    connection_args = EsxiConnectonArgs(
        hostname=args.hostname,
        username=args.username,
        password_file=args.password_file,
        skip_cert_verification=args.skip_cert_verification,
    )

    with connect_to_esxi_host(connection_args) as connection:
        data = {}
        for vm in list_vms(connection):
            try:
                fetch_and_update_vm_data(vm, data)
            except Exception as err:
                print(
                    f"Failed to get info for VM {vm.name}: {err}",
                    file=sys.stderr,
                )

    json.dump(data, sys.stdout, indent=2, default=json_dump_helper)


if __name__ == "__main__":
    try:
        main()
    except Exception as err:
        print(err, file=sys.stderr)
        sys.exit(1)
