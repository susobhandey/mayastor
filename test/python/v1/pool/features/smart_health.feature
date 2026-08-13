Feature: Pool disk SMART/health reporting

  Background:
    Given a mayastor instance "ms0"

  Scenario: querying health for a pool backed by an in-memory bdev
    Given a pool "p0" on "disk0"
    When the user queries the pool health
    Then the health query should succeed
    And the disk health should be reported as not supported

  Scenario: querying health for an aio pool backed by a plain file
    When the user creates a pool specifying a URI representing an aio disk
    And the user queries the pool health
    Then the health query should succeed
    And the disk health should be reported as not supported

  Scenario: querying health for a non-existent pool
    When the user queries the health of a pool that does not exist
    Then the pool health query should fail

  @requires_kernel_disk
  Scenario: querying health for a pool on a real kernel-attached disk
    Given a pool on a real kernel-attached disk
    When the user queries the pool health
    Then the health query should succeed
    And the disk health should be reported as supported
    And the reported health should match smartctl for the real disk
    And the reported device identity should match smartctl for the real disk

  @requires_vfio_disk
  Scenario: querying health for a pool on a VFIO-attached NVMe disk
    Given a pool on a real VFIO-attached NVMe disk
    When the user queries the pool health
    Then the health query should succeed
    And the disk health should be reported as supported
    And the reported device identity should include a model and serial number
    And the reported SMART attribute table should be empty
