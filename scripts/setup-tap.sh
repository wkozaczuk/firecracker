ip tuntap add veth0 mode tap
ifconfig veth0 192.168.1.231/24 up

#brctl stp virbr0 off
#brctl addif virbr0 veth0
#ifconfig veth0 up
