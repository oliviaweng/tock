# Summary
This repo contains the edited TockOS with the PeripheralManager for power management. Below lists the files changed, with a summary of the changes. The individual files also have comments surrounding where changes were made explaining them - you can Ctrl+F and search for JWINK to find the comments. 

# Files Changed

## Kernel Extensions

**kernel/src/utilities/peripheral_management.rs**: Add PeripheralDevice and SubscriptionManager traits, and GeneralPeripheralManager struct. Note that in the report/slides the GeneralPeripheralManager is just referred to as the PeripheralManager. 

**kernel/src/syscall_driver.rs**: Added the function subscription_changed to the SyscallDriver trait with an empty default implementation. 

**kernel/src/kernel.rs**: When an application subscribes/unsubscribes from a device driver's events, call the drivers subscription_changed function.

**kernel/src/grant.rs**: Add has_upcall function to GrantKernelData struct, allows drivers to check whether or not any applications are currently subscribed to a particular event. 

## Board Specific

**boards/components/src/ieee802154.rs**: Add SubscriptionManager trait to Radio definition so subscription events can be passed to the lowest level radio driver. 

**boards/nordic/nrf52840dk/src/main.rs**: Turn radio off after initialization.

## Radio Driver Stack

**capsules/src/ieee802154/device.rs**: Add subscriber_added and subscriber_removed functions to MacDevice trait.

**capsules/src/ieee802154/driver.rs**: Highest level radio driver. Implement subscription_changed function to receive events from the kernel, check the GrantKernelData for existing Rx subscribers, and forward data down to a lower level driver using the subscriber_added and subscriber_removed functions.

**capsules/src/ieee802154/virtual_mac.rs**: Implement subscriber_added and subscriber_removed functions to forward subscription events down to a lower level driver.

**capsules/src/ieee802154/framer.rs**: Implement subscriber_added and subscriber_removed functions to forward subscription events down to a lower level driver.

**capsules/src/ieee802154/mac.rs**: Add subscriber_added and subscriber_removed functions to Mac trait. Implement them for AwakeMac struct to forward subscription events to the lowest level driver. 

**chips/nrf52/src/ieee802154_radio.rs**: Lowest level radio driver, rewrite functionality to use the GeneralPeripheralManager.


