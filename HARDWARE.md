# Hardware

## Fermion 8421 Encoder

SKU: DFR0721

### Wiring

| Pi     | Pi Pin Number | Fermion 8421 |
|--------|---------------|--------------|
| Ground | 9             | C            |
| 3.3V   | 17            | X            |
| GPIO17 | 11            | 8            |
| GPIO27 | 13            | 1            |
| GPIO22 | 15            | 2            |
| GPIO20 | 38            | 4            |


Operating mode:

| Position | Bits (8:4:2:1)  | Mode      |
|----------|-----------------|-----------|
| `0x8`    | 8:1 4:0 2:0 1:0 | Radio    |
| `0x7`    | 8:0 4:1 2:1 1:1 | Clock    |
| `0x6`    | 8:0 4:1 2:1 1:0 | Streamer |

Any other position falls back to Radio mode.

## Adafruit I2C Stemma QT Rotary Encoder

* Product ID: 5880

| I2C   | EQ     | Volume | Select |
|-------|--------|--------|--------|
| ADDR  | `0x3A` | `0x37` | `0x36` |


## I2C vacuum fluorescent display (VFD)

* [Documentation](https://nicolaielectronics.nl/docs/ne-hcs12ss59t-r1_i2c_vfd/)
* [Firmware](https://github.com/Nicolai-Electronics/i2c-vfd-firmware)
* I2C ADDR: `0x10`
