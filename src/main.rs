use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use image::{DynamicImage, GenericImageView, Rgba, imageops::FilterType};
use tempdir::TempDir;
use tesseract::Tesseract;

const CHART_WIDTH: u32 = 700;
const CHART_HEIGHT: u32 = 250;
const CHART_HEIGHT_PX: f32 = 136.0;

/// The type of writer to use for output
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq)]
enum WriterType {
    /// Write output to the console
    Console,
    /// Write output to InfluxDB
    Influx,
}

/// A tool to extract energy production data from SunnyPortal daily chart images
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the daily chart image file
    #[arg()]
    chart_file: String,
    /// Show total and a summary of the data
    #[arg(short = 't', long = "total")]
    total: bool,
    /// Writer to use for output
    #[arg(short = 'w', long = "writer", value_enum, default_value_t = WriterType::Console)]
    writer: WriterType,
}

/// An iterator that goes through the pixels of a vertical line in the image
/// This iterator is used to extract pixel data from a specific x-coordinate
/// in the image, which is useful for analyzing the chart data.
struct VertialPixelIterator<'a> {
    img: &'a image::DynamicImage,
    x: u32,
    y: u32,
}

impl<'a> VertialPixelIterator<'a> {
    /// Creates a new `VertialPixelIterator` for the given image and x-coordinate.
    ///
    /// # Arguments
    /// * `img` - A reference to the image from which pixels will be iterated
    /// * `x` - The x-coordinate of the vertical line in the image to iterate
    ///
    /// # Returns
    /// A new instance of `VertialPixelIterator` initialized with the image and x-coordinate
    fn new(img: &'a image::DynamicImage, x: u32) -> Self {
        Self { img, x, y: 0 }
    }
}

/// An iterator that goes through the pixels of a vertical line in the image
/// This iterator is used to extract pixel data from a specific x-coordinate
/// in the image, which is useful for analyzing the chart data.
impl Iterator for VertialPixelIterator<'_> {
    type Item = Rgba<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.y < self.img.height() {
            let pixel = self.img.get_pixel(self.x, self.y);
            self.y += 1;
            Some(pixel)
        } else {
            None
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.img.height() - self.y;
        (remaining as usize, Some(remaining as usize))
    }
}

struct HourlyPowerGeneration<'a> {
    /// A reference to the image from which the hourly power generation data will be extracted
    img: &'a DynamicImage,
    /// The maximum watts on the charts y-axis
    maximum: f32,
}

impl<'a> HourlyPowerGeneration<'a> {
    /// Creates a new `HourlyPowerGeneration` instance.
    ///
    /// # Returns
    /// A new instance of `HourlyPowerGeneration`
    pub fn from_image(img: &'a DynamicImage) -> Result<Self> {
        Ok(Self {
            img,
            maximum: Self::maximum_watts_in_chart(img)?,
        })
    }

    /// Returns the maximum watts in the chart by counting the number of gray pixels
    /// in the vertical line at `CHART_X_OFFSET_FOR_COUNTING_MAXIMUM_WATTS`.
    /// The gray color is used to represent the maximum watts in the chart.
    ///
    /// # Arguments
    /// * `img` - A reference to the image from which the maximum watts will be calculated
    /// # Returns
    /// The maximum watts in the chart unit Watt
    fn maximum_watts_in_chart(img: &DynamicImage) -> Result<f32> {
        let tmp_dir = TempDir::new("daily-chart")?;
        let file_path = tmp_dir.path().join("ocr.png");

        // Prepare the image for OCR by cropping, resizing, converting to grayscale, and adjusting contrast.
        img.clone()
            .crop(2, 14, 52, 20)
            // The values here are based on experimentation and observation of the chart images.
            .resize(150, 200, FilterType::Triangle)
            .grayscale()
            .adjust_contrast(-256.0)
            .to_rgb8()
            .save(&file_path)?;

        let ocr = Tesseract::new(None, Some("eng"))?;
        let mut image_set = ocr.set_image(file_path.to_str().unwrap()).unwrap();
        let watts = image_set
            .get_text()
            .with_context(|| "Failed to extract text from the image using OCR")?
            .trim()
            .parse::<f32>()
            .with_context(|| "Failed to parse the extracted text as a number")?;
        Ok(watts * 1000.0)
    }

    fn generation_in_watts(&self, x: u32) -> Result<u32> {
        let dark_blue = image::Rgba([29, 75, 145, 255]);
        let vertical_pixel_iter = VertialPixelIterator::new(self.img, x);
        let dark_blue_pixels: Vec<_> = vertical_pixel_iter
            .enumerate()
            .filter(|x| x.1 == dark_blue)
            .collect();
        match dark_blue_pixels.len() {
            0 => anyhow::bail!("No dark blue pixels found at x={}", x),
            1 => Ok(0), // If there's only one dark blue pixel, we assume no generation
            2 => {
                let first = dark_blue_pixels.first().unwrap();
                let last = dark_blue_pixels.last().unwrap();
                let value = self.maximum / CHART_HEIGHT_PX * (last.0 - first.0) as f32;
                Ok(value.ceil() as u32) // Round up to the nearest whole number
            }
            _ => anyhow::bail!("Too many dark blue pixels found at x={}", x),
        }
    }

    pub fn hours_watts(&self) -> Result<Vec<(u32, u32)>> {
        let x_offset: Vec<u32> = (64..(24 * 64)).step_by(26).collect();
        let hours: Vec<u32> = (0..24).collect();

        hours
            .iter()
            .zip(x_offset)
            .map(|(hour, x_pixel_offset)| {
                self.generation_in_watts(x_pixel_offset)
                    .map(|watts| (*hour, watts))
            })
            .collect()
    }

    pub fn total_watthours(&self) -> Result<f64> {
        let total_wh = self
            .hours_watts()?
            .iter()
            .fold(0, |acc, (_hour, watts)| acc + watts);
        Ok((total_wh as f64).ceil())
    }
}

/// Trait for writing the hourly power generation data
///
/// Implement this trait for differnt output formats, such as console or InfluxDB.
///
/// # Arguments
///
/// * `data` - The hourly power generation data to write
/// * `write_total` - A boolean indicating whether to write the total and summary of the data
///
///  # Returns
/// A `Result` indicating success or failure of the write operation
trait Writer {
    fn write(&self, data: &HourlyPowerGeneration, write_total: bool) -> Result<()>;
}

/// A writer that outputs the hourly power generation data to the console
struct ConsoleWriter;

impl Writer for ConsoleWriter {
    fn write(
        &self,
        hourly_power_generation: &HourlyPowerGeneration,
        write_total: bool,
    ) -> Result<()> {
        let hours_and_watts = hourly_power_generation.hours_watts()?;
        hours_and_watts.iter().for_each(|(hour_of_day, watts)| {
            println!("{:02} {:5} Wh", hour_of_day, watts);
        });

        if write_total {
            println!(
                "\nMaximum Watt in chart: {} Wh",
                hourly_power_generation.maximum
            );

            let total_watthours = hourly_power_generation.total_watthours()?;
            println!("Total: {} Wh", total_watthours);
        }

        Ok(())
    }
}

/// A writer that outputs the hourly power generation to InfluxDB format
struct InfluxWriter;

impl Writer for InfluxWriter {
    fn write(
        &self,
        hourly_power_generation: &HourlyPowerGeneration,
        _write_total: bool,
    ) -> Result<()> {
        let measurement = "sunnyportal_daily_chart";
        let field = "generation";

        let hours_and_watts = hourly_power_generation.hours_watts()?;
        hours_and_watts.iter().for_each(|(hour_of_day, watts)| {
            println!("{} {}={} {}", measurement, field, watts, hour_of_day);
        });

        Ok(())
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    let img = image::open(&args.chart_file)
        .with_context(|| format!("Failed to load file {}", &args.chart_file))?;

    if img.dimensions() != (CHART_WIDTH, CHART_HEIGHT) {
        return Err(anyhow::anyhow!(
            "Image dimensions are not {CHART_WIDTH}x{CHART_HEIGHT}"
        ));
    }

    // Choose the writer based on the command-line argument
    let writer: Box<dyn Writer> = match args.writer {
        WriterType::Influx => Box::new(InfluxWriter),
        WriterType::Console => Box::new(ConsoleWriter),
    };

    let hourly_power_generation = HourlyPowerGeneration::from_image(&img)?;
    writer.write(&hourly_power_generation, args.total)?;

    Ok(())
}
