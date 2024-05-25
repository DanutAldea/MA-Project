#![no_std]
#![no_main]

use core::fmt::Write;

use cyw43_pio::PioSpi;
use eeprom24x::page_size::No;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, IpAddress, Ipv4Address, Ipv4Cidr, Stack, StackResources};
use embassy_rp::adc::{Adc, Config as AdcConfig, InterruptHandler as AdcInterruptHandler};
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Input, Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0, USB};
use embassy_rp::pio::{InterruptHandler as PioInterruptHandler, Pio};
use embassy_rp::usb::{Driver, InterruptHandler as USBInterruptHandler};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write as _;
// use futures::TryFutureExt;
use heapless::String;
use lcd1602_driver::lcd::{Basic, Ext};
use static_cell::StaticCell;

// Panic Probe imports
use panic_probe as _;
use defmt_rtt as _;
use defmt::*;

// I2C
use embassy_rp::i2c::{Config as I2cConfig, I2c, InterruptHandler as I2CInterruptHandler};
use embassy_rp::peripherals::I2C0;

// TODO: Change these depending on network
pub mod network_constants {
    pub const WIFI_NETWORK: &str = "Cristina";
    pub const WIFI_PASSWORD: &str = "cristina24091983";
    pub const REMOTE_IP_ADDRESS: embassy_net::Ipv4Address = embassy_net::Ipv4Address::new(192, 168, 100, 212);
    pub const TURRET_IP_ADDRESS: embassy_net::Ipv4Address = embassy_net::Ipv4Address::new(192, 168, 100, 2);
    pub const PORT: u16 = 3000;
    pub const PREFIX_LEN: u8 = 24;
    pub const DEFAULT_GATEWAY: Option<embassy_net::Ipv4Address> = Some(embassy_net::Ipv4Address::new(192, 168, 100, 1));
}

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    // USBCTRL_IRQ => USBInterruptHandler<USB>;
    ADC_IRQ_FIFO => AdcInterruptHandler;
    I2C0_IRQ => I2CInterruptHandler<I2C0>;
});

#[embassy_executor::task]
async fn wifi_task(
    runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<cyw43::NetDriver<'static>>) -> ! {
    stack.run().await
}

const LCD_ADDRESS: u8 = 0x27;

#[derive(Clone, Copy, Debug)]
pub enum Direction {
    Up = 100,
    Down = 101,
    Left = 102,
    Right = 103,
    UpLeft = 104,
    UpRight = 105,
    DownLeft = 106,
    DownRight = 107,
}

impl Direction {
    pub fn from_samples(x: u16, y: u16) -> Option<Direction> {
        let mut dir = None;

        if x < 250 {
            dir = Some(Direction::Left);
        } else if x > 2500 {
            dir = Some(Direction::Right);
        }

        if y < 250 {
            dir = match dir {
                Some(Direction::Left) => Some(Direction::DownLeft),
                Some(Direction::Right) => Some(Direction::DownRight),
                _ => Some(Direction::Down),
            }
        } else if y > 2500 {
            dir = match dir {
                Some(Direction::Left) => Some(Direction::UpLeft),
                Some(Direction::Right) => Some(Direction::UpRight),
                _ => Some(Direction::Up),
            }
        }

        dir
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Packet {
    Shoot,
    Move(Direction),
    None
}

impl Packet {
    pub fn to_u8(&self) -> u8 {
        match self {
            Packet::Shoot => 123,
            Packet::Move(m) => *m as u8,
            Packet::None => 99
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    defmt::println!("Hello!");
    info!("Hello!");

    let p = embassy_rp::init(Default::default());

    let fw = include_bytes!("../../cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../../cyw43-firmware/43439A0_clm.bin");

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        p.DMA_CH0,
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    spawner.spawn(wifi_task(runner)).unwrap();

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    // Use a link-local address for communication without DHCP server
    let config = Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: Ipv4Cidr::new(network_constants::REMOTE_IP_ADDRESS, network_constants::PREFIX_LEN),
        dns_servers: heapless::Vec::new(),
        gateway: network_constants::DEFAULT_GATEWAY,
    });

    // Generate random seed
    let seed = 0x0132_6745_8ba9_dcf0; // chosen by fair dice roll. guaranteed to be random.

    // Init network stack
    static STACK: StaticCell<Stack<cyw43::NetDriver<'static>>> = StaticCell::new();
    static RESOURCES: StaticCell<StackResources<2>> = StaticCell::new();
    let stack = &*STACK.init(Stack::new(
        net_device,
        config,
        RESOURCES.init(StackResources::<2>::new()),
        seed,
    ));

    // Spawn network task
    spawner.spawn(net_task(stack)).unwrap();
    control.gpio_set(0, true).await;

    // Configure I2C for LCD 1602
    let sda = p.PIN_4;
    let scl = p.PIN_5;

    let mut i2c_conf = I2cConfig::default();
    i2c_conf.frequency = 100_000;
    let mut i2c = I2c::new_blocking(p.I2C0, scl, sda, i2c_conf);
    control.gpio_set(0, false).await;

    // Setup LCD
    let mut delayer = embassy_time::Delay;
    let mut sender = lcd1602_driver::sender::I2cSender::new(&mut i2c, LCD_ADDRESS);
    let lcd_config = lcd1602_driver::lcd::Config::default()
        .set_data_width(lcd1602_driver::command::DataWidth::Bit4);
    let mut lcd = lcd1602_driver::lcd::Lcd::new(&mut sender, &mut delayer, lcd_config, 10);
    lcd.set_cursor_blink_state(lcd1602_driver::command::State::On);

    // lcd.write_str_to_cur("Hello!");
    // Timer::after_secs(1).await;

    // Configure ADC and switch for Joystick
    let mut adc = Adc::new(p.ADC, Irqs, AdcConfig::default());
    let mut x = embassy_rp::adc::Channel::new_pin(p.PIN_26, embassy_rp::gpio::Pull::None);
    let mut y = embassy_rp::adc::Channel::new_pin(p.PIN_27, embassy_rp::gpio::Pull::None);
    let trigger = Input::new(p.PIN_28, embassy_rp::gpio::Pull::Up);

    lcd.clean_display();
    lcd.write_str_to_pos(" Connecting  to ", (0, 0));
    lcd.write_str_to_pos("      WiFi      ", (0, 1));
    control.gpio_set(0, true).await; // Turn LED on

    loop {
        if control.join_wpa2(network_constants::WIFI_NETWORK, network_constants::WIFI_PASSWORD).await.is_ok() {
            break;
        }
    }

    lcd.clean_display();
    lcd.write_str_to_pos("   Connected!   ", (0, 0));
    Timer::after_secs(1).await;
    control.gpio_set(0, false).await; // Turn LED off

    let mut rx_buffer = [0; 1024];
    let mut tx_buffer = [0; 1024];

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));
        defmt::println!("Configured TCP Socket!");

        // Configure IP address and Port
        let endpoint = (network_constants::TURRET_IP_ADDRESS, network_constants::PORT);

        control.gpio_set(0, false).await; // Turn LED on
        if let Err(e) = socket.connect(endpoint).await {
            defmt::println!("accept error: {:?}", e);
            continue;
        }
        control.gpio_set(0, true).await; // Turn LED off

        loop {
            // Read ADC
            let pos_x = adc.read(&mut x).await;
            let pos_y = adc.read(&mut y).await;

            let mut first_line: String<16> = String::new();
            let mut second_line: String<16> = String::new();

            let mut pack: Packet = Packet::None;

            if trigger.is_low() {
                pack = Packet::Shoot;
                defmt::println!("Shoot!");

                lcd.clean_display();
                lcd.write_str_to_pos("     Shoot!     ", (0, 0));
            } else {
                match (pos_x, pos_y) {
                    (Err(_), _) | (_, Err(_)) => warn!("Conversion failed!"),
                    (Ok(pos_x), Ok(pos_y)) => {
                        defmt::println!("x = {} y = {}", pos_x, pos_y);
                        core::write!(&mut first_line, "    x = {}    ", pos_x).unwrap();
                        core::write!(&mut second_line, "    y = {}    ", pos_y).unwrap();

                        pack = match Direction::from_samples(pos_x, pos_y) {
                            None => Packet::None,
                            Some(dir) => Packet::Move(dir)
                        };

                        lcd.clean_display();
                        lcd.write_str_to_pos(&first_line, (0, 0));
                        lcd.write_str_to_pos(&second_line, (0, 1));
                    }
                }
            }

            match socket.write_all(&[pack.to_u8()]).await {
                Ok(_) => (),
                Err(e) => {
                    defmt::println!("Package transmission error: {:?}", e);
                    break;
                }
            }

            Timer::after_millis(500).await;
        }
    }
}
