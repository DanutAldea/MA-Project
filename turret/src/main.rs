#![no_std]
#![no_main]

// use core::fmt::Write;

use cyw43_pio::PioSpi;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
// use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, Ipv4Cidr, Stack, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::{InterruptHandler as PioInterruptHandler, Pio};
use embassy_rp::pwm::{Pwm, Config as PwmConfig};
use embassy_time::{Duration, Timer};
use fixed::types::extra::U4;
use fixed::{types, FixedU16};
use static_cell::StaticCell;

// Panic Probe imports
use panic_probe as _;
use defmt_rtt as _;
use defmt::*;

// TODO: Change these depending on network
pub mod network_constants {
    pub const WIFI_NETWORK: &str = "Wyliodrin";
    pub const WIFI_PASSWORD: &str = "g3E2PjWy";
    pub const REMOTE_IP_ADDRESS: embassy_net::Ipv4Address = embassy_net::Ipv4Address::new(192, 168, 1, 212);
    pub const TURRET_IP_ADDRESS: embassy_net::Ipv4Address = embassy_net::Ipv4Address::new(192, 168, 1, 213);
    pub const PORT: u16 = 3000;
    pub const PREFIX_LEN: u8 = 24;
    pub const DEFAULT_GATEWAY: Option<embassy_net::Ipv4Address> = Some(embassy_net::Ipv4Address::new(192, 168, 1, 1));
    // pub const WIFI_NETWORK: &str = "motorola312";
    // pub const WIFI_PASSWORD: &str = "nsog0632";
    // pub const REMOTE_IP_ADDRESS: embassy_net::Ipv4Address = embassy_net::Ipv4Address::new(192, 168, 133, 92);
    // pub const TURRET_IP_ADDRESS: embassy_net::Ipv4Address = embassy_net::Ipv4Address::new(192, 168, 133, 93);
    // pub const PORT: u16 = 3000;
    // pub const PREFIX_LEN: u8 = 24;
    // pub const DEFAULT_GATEWAY: Option<embassy_net::Ipv4Address> = Some(embassy_net::Ipv4Address::new(192, 168, 133, 90));
}

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
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

#[derive(Clone, Copy, Debug)]
pub enum Direction {
    Left = 108,
    Right = 114,
}

impl Direction {
    pub fn from_samples(x: u16, y: u16) -> Option<Direction> {
        if x < 250 {
            Some(Direction::Left)
        } else if x > 2500 {
            Some(Direction::Right)
        } else {
            None
        }
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
            Packet::Shoot => 115,
            Packet::Move(m) => *m as u8,
            Packet::None => 110
        }
    }

    pub const fn from_u8(byte: u8) -> Option<Packet> {
        match byte {
            115 => Some(Packet::Shoot),
            110 => Some(Packet::None),
            108 => Some(Packet::Move(Direction::Left)),
            114 => Some(Packet::Move(Direction::Right)),
            _ => None
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
        address: Ipv4Cidr::new(network_constants::TURRET_IP_ADDRESS, network_constants::PREFIX_LEN),
        dns_servers: heapless::Vec::new(),
        gateway: network_constants::DEFAULT_GATEWAY,
    });

    // Generate random seed
    let seed = 0x6aa0_4d9d_1f12_6d0a; // chosen by fair dice roll. guaranteed to be random.

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

    loop {
        // control.join_open(network_constants::WIFI_NETWORK).await.is_ok() {
        if control.join_wpa2(network_constants::WIFI_NETWORK, network_constants::WIFI_PASSWORD).await.is_ok() {
            defmt::println!("Connected!");
            break;
        }
    }

    // Configure PWM
    let mut trigger_config = PwmConfig::default();
    trigger_config.top = 25000 - 1;
    trigger_config.divider = FixedU16::<U4>::from_num(100);
    let mut pwm = Pwm::new_output_a(p.PWM_SLICE0, p.PIN_16, trigger_config.clone());

    let mut rotation_config = PwmConfig::default();
    rotation_config.top = 25000 - 1;
    rotation_config.compare_a = 1875;
    rotation_config.divider = FixedU16::<U4>::from_num(100);
    let mut rotation = Pwm::new_output_a(p.PWM_SLICE3, p.PIN_22, rotation_config.clone());

    let mut motor_config = PwmConfig::default();
    motor_config.top = 100;
    motor_config.compare_a = 0;
    // let _in1 = Output::new(p.PIN_18, Level::High);
    let mut motor1 = Pwm::new_output_a(p.PWM_SLICE1, p.PIN_18, motor_config.clone());
    let _in2 = Output::new(p.PIN_19, Level::Low);
    // let _in3 = Output::new(p.PIN_20, Level::High);
    let mut motor2 = Pwm::new_output_a(p.PWM_SLICE2, p.PIN_20, motor_config.clone());
    let _in4 = Output::new(p.PIN_21, Level::Low);
    Timer::after_secs(1).await;

    let mut rx_buffer = [0; 1024];
    let mut tx_buffer = [0; 1024];
    
    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    socket.set_timeout(Some(Duration::from_secs(10)));
    defmt::println!("Configured TCP Socket!");

    while let Err(e) = socket.accept(network_constants::PORT).await {
        defmt::println!("Accept error {}", e);
    }
    control.gpio_set(0, true).await;
    let mut buf= [0];

    loop {
        match socket.read(&mut buf).await {
            Ok(0) => {
                defmt::println!("read EOF");
                break;  
            },
            Ok(_n) => {
                let pack = Packet::from_u8(buf[0]);

                match pack {
                    None | Some(Packet::None) => continue,
                    Some(Packet::Shoot) => break,
                    Some(Packet::Move(Direction::Right)) => {
                        if rotation_config.compare_a - 125 >= 625 {
                            rotation_config.compare_a -= 125;
                            rotation.set_config(&rotation_config);
                        }
                    },
                    Some(Packet::Move(Direction::Left)) => {
                        if rotation_config.compare_a + 125 <= 3125 {
                            rotation_config.compare_a += 125;
                            rotation.set_config(&rotation_config);
                        }
                    }
                };
            },
            Err(e) => {
                core::panic!("Read error :(((\n{:?}", e);
            }
        };
    }
    
    motor_config.compare_a = 80;
    motor1.set_config(&motor_config);
    motor2.set_config(&motor_config);

    for _ in 0..20 {
        trigger_config.compare_a = 1875;
        pwm.set_config(&trigger_config);
        Timer::after_millis(200).await;
        trigger_config.compare_a = 3125;
        pwm.set_config(&trigger_config);
        Timer::after_millis(200).await;
    }

    trigger_config.compare_a = 1875;
    pwm.set_config(&trigger_config);

    // Slow stop
    motor_config.compare_a = 60;
    motor1.set_config(&motor_config);
    motor2.set_config(&motor_config);
    Timer::after_millis(200).await;
    motor_config.compare_a = 40;
    motor1.set_config(&motor_config);
    motor2.set_config(&motor_config);
    Timer::after_millis(200).await;
    motor_config.compare_a = 20;
    motor1.set_config(&motor_config);
    motor2.set_config(&motor_config);
    Timer::after_millis(200).await;
    motor_config.compare_a = 0;
    motor1.set_config(&motor_config);
    motor2.set_config(&motor_config);
    Timer::after_millis(200).await;

}
