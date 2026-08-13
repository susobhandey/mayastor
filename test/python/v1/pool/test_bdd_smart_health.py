"""Pool disk SMART/health reporting feature tests.

The last two scenarios (kernel-attached and VFIO-attached real disks) need
hardware the standard docker-compose `ms0` service doesn't expose (no /dev
bind mount, no /dev/vfio, not privileged) and so are gated behind env vars,
following the same pattern as test/python/tests/rpc/test_interrupt_mode.py's
ENABLE_INTERRUPT_MODE gate:

  SMARTHEALTH_DISK_PATH   /dev/disk/by-id/... path of a real kernel-attached
                          disk (aio://), for the kernel-backed scenario.
  SMARTHEALTH_VFIO_BDF    PCI BDF (e.g. 0000:89:00.0) of an NVMe device
                          already bound to vfio-pci, for the VFIO scenario.

Running them also requires overriding docker-compose.yml to bind-mount /dev
(and run privileged for the VFIO case) so ms0 can actually reach the device.
"""

import json
import os

import grpc
import pytest
from pytest_bdd import given, parsers, scenario, then, when

import pool_pb2 as pb
from common.command import run_cmd
from v1.mayastor import container_mod, mayastor_mod

DISK_PATH = os.environ.get("SMARTHEALTH_DISK_PATH", "")
VFIO_BDF = os.environ.get("SMARTHEALTH_VFIO_BDF", "")

kernel_disk_only = pytest.mark.skipif(
    not DISK_PATH,
    reason="no real kernel-attached disk (SMARTHEALTH_DISK_PATH not set)",
)
vfio_disk_only = pytest.mark.skipif(
    not VFIO_BDF,
    reason="no real VFIO-attached NVMe disk (SMARTHEALTH_VFIO_BDF not set)",
)


@scenario(
    "features/smart_health.feature",
    "querying health for a pool backed by an in-memory bdev",
)
def test_querying_health_for_a_pool_backed_by_an_in_memory_bdev():
    """querying health for a pool backed by an in-memory bdev."""


@scenario(
    "features/smart_health.feature",
    "querying health for an aio pool backed by a plain file",
)
def test_querying_health_for_an_aio_pool_backed_by_a_plain_file():
    """querying health for an aio pool backed by a plain file."""


@scenario(
    "features/smart_health.feature", "querying health for a non-existent pool"
)
def test_querying_health_for_a_non_existent_pool():
    """querying health for a non-existent pool."""


@kernel_disk_only
@scenario(
    "features/smart_health.feature",
    "querying health for a pool on a real kernel-attached disk",
)
def test_querying_health_for_a_pool_on_a_real_kernel_attached_disk():
    """querying health for a pool on a real kernel-attached disk."""


@vfio_disk_only
@scenario(
    "features/smart_health.feature",
    "querying health for a pool on a VFIO-attached NVMe disk",
)
def test_querying_health_for_a_pool_on_a_vfio_attached_nvme_disk():
    """querying health for a pool on a VFIO-attached NVMe disk."""


@pytest.fixture
def image_file():
    name = "/tmp/ms0-smarthealth-disk0.img"
    run_cmd(f"rm -f '{name}'", True)
    run_cmd(f"truncate -s 64M '{name}'", True)
    yield name
    run_cmd(f"rm -f '{name}'", True)


@pytest.fixture
def replica_pools(get_mayastor_instance):
    pools = {}
    yield pools
    for name in pools.keys():
        opts = pb.ListPoolOptions()
        opts.name.value = name
        found = get_mayastor_instance.pool_rpc.ListPools(opts).pools
        if len(found) != 0:
            opts = pb.DestroyPoolRequest()
            opts.name = found[0].name
            opts.uuid.value = found[0].uuid
            get_mayastor_instance.pool_rpc.DestroyPool(opts)


@pytest.fixture
def create_pool(get_mayastor_instance, replica_pools):
    def create(name, disks, uuid=None):
        opts = pb.CreatePoolRequest(name=name, disks=disks)
        if uuid is not None:
            opts.uuid.value = uuid
        pool = get_mayastor_instance.pool_rpc.CreatePool(opts)
        replica_pools[name] = pool
        return pool

    yield create


@pytest.fixture
def find_pool(get_mayastor_instance):
    def find(name):
        for pool in get_mayastor_instance.pool_rpc.ListPools(
            pb.ListPoolOptions()
        ).pools:
            if pool.name == name:
                return pool
        return None

    yield find


@given(
    parsers.parse('a mayastor instance "{name}"'),
    target_fixture="get_mayastor_instance",
)
def get_mayastor_instance(mayastor_mod, name):
    return mayastor_mod[name]


@given(parsers.parse('a pool "{name}" on "{disk}"'), target_fixture="pool_name")
def pool_name_on_disk(create_pool, name, disk):
    create_pool(name, [f"malloc:///{disk}?size_mb=100"], None)
    return name


@kernel_disk_only
@given("a pool on a real kernel-attached disk", target_fixture="pool_name")
def pool_on_real_kernel_disk(create_pool):
    create_pool("p0", [f"aio://{DISK_PATH}"])
    return "p0"


@vfio_disk_only
@given("a pool on a real VFIO-attached NVMe disk", target_fixture="pool_name")
def pool_on_real_vfio_disk(create_pool):
    create_pool("p0", [f"pcie:///{VFIO_BDF}"])
    return "p0"


@when("the user creates a pool specifying a URI representing an aio disk")
def create_pool_from_aio_disk(create_pool, image_file):
    create_pool("p0", [f"aio://{image_file}"])


@when("the user queries the pool health", target_fixture="pool_health")
def query_pool_health(get_mayastor_instance, create_pool, replica_pools):
    # Whichever pool was set up by a preceding Given step -- in this suite
    # there's always exactly one at this point.
    name = next(iter(replica_pools.keys()))
    return get_mayastor_instance.pool_rpc.GetPoolHealth(
        pb.GetPoolHealthRequest(name=name)
    )


@when(
    "the user queries the health of a pool that does not exist",
    target_fixture="pool_health_error",
)
def query_health_of_missing_pool(get_mayastor_instance):
    with pytest.raises(grpc.RpcError) as error:
        get_mayastor_instance.pool_rpc.GetPoolHealth(
            pb.GetPoolHealthRequest(name="no-such-pool")
        )
    return error


@then("the health query should succeed")
def health_query_should_succeed(pool_health):
    assert pool_health is not None
    # Pools are single-disk today, so exactly one health entry is expected.
    assert len(pool_health.disks) == 1


@then("the pool health query should fail")
def pool_health_query_should_fail(pool_health_error):
    assert pool_health_error.value.code() == grpc.StatusCode.NOT_FOUND


@then("the disk health should be reported as not supported")
def disk_health_not_supported(pool_health):
    assert pool_health.disks[0].supported is False
    assert not pool_health.disks[0].HasField("health")


@then("the disk health should be reported as supported")
def disk_health_supported(pool_health):
    assert pool_health.disks[0].supported is True
    assert pool_health.disks[0].HasField("health")


def _real_smartctl_json():
    out = run_cmd(f"sudo smartctl --json --all '{DISK_PATH}'", True, ignore_errors=True)
    return json.loads(out)


@then("the reported health should match smartctl for the real disk")
def health_matches_smartctl(pool_health):
    j = _real_smartctl_json()
    log = j["nvme_smart_health_information_log"]
    health = pool_health.disks[0].health
    assert health.critical_warning == log["critical_warning"]
    assert health.power_cycles == log["power_cycles"]
    assert health.power_on_hours == log["power_on_hours"]
    assert health.available_spare_percent == log["available_spare"]
    assert health.media_errors == log["media_errors"]


@then("the reported device identity should match smartctl for the real disk")
def identity_matches_smartctl(pool_health):
    j = _real_smartctl_json()
    identity = pool_health.disks[0].health.identity
    assert identity.model == j["model_name"]
    assert identity.serial_number == j["serial_number"]
    assert identity.firmware_revision == j["firmware_version"]
    assert identity.capacity_bytes == j["user_capacity"]["bytes"]


@then("the reported device identity should include a model and serial number")
def identity_has_model_and_serial(pool_health):
    identity = pool_health.disks[0].health.identity
    assert identity.model != ""
    assert identity.serial_number != ""
    assert identity.firmware_revision != ""


@then("the reported SMART attribute table should be empty")
def smart_attribute_table_is_empty(pool_health):
    assert len(pool_health.disks[0].health.smart_attributes) == 0
