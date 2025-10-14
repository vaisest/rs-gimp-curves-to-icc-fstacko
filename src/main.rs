use clap::{Args, Parser};
use itertools::Itertools;
use lcms2::{Locale, Profile, Tag, ToneCurve, MLU};
use regex::Regex;
use std::{fs, path::PathBuf};

#[derive(Parser)]
#[command(name = "GIMP Curve to ICC")]
struct Arguments {
    #[command(flatten)]
    input: InputGroup,

    /// Output file name
    #[arg(default_value = "out.icc")]
    icc_output: PathBuf,

    /// Description or name that will appear in Windows' colour management menu
    #[arg(
        short,
        long = "description",
        default_value = "Custom gamma ICC profile"
    )]
    description: String,
}

#[derive(Args)]
#[group(required = true, multiple = false)]
struct InputGroup {
    /// Input file name
    #[arg(short, long)]
    input_curves: Option<PathBuf>,

    /// Desired gamma value. 1 is equal to doing nothing, <1 makes the image darker, and >1 makes the image brighter.
    #[arg(short, long)]
    gamma: Option<f32>,
}

/// Parses e.g. "0.0 0.001 0.033 ..." to u16 representing the same [0, 1] domain
fn parse_curve(input: &str) -> Vec<u16> {
    input
        .split(" ")
        .map(|it| it.parse::<f64>().expect("failed to parse number"))
        .inspect(|&v| {
            assert!(
                (0.0..=1.0).contains(&v),
                "malformed input: curve value outside [0, 1]"
            )
        })
        .map(|f| (f * (u16::MAX) as f64).round() as u16)
        .collect()
}

/// Scales from 0-65535 to 0-255
fn scale_u16_to_u8_range(input: u16) -> u8 {
    ((input as f64 / u16::MAX as f64) * u8::MAX as f64) as u8
}

/// Parses GIMP's new curve format which is formatted in a LISP-like way
fn parse_curves(text: String) -> Vec<Vec<u16>> {
    // gimp seems to be able to save linear curves which will probably look wrong
    if text.contains("linear yes") {
        println!("Curve input is saved in linear light. The result might not look correct")
    }

    // mR flags: multi-line and CRLF mode
    let re = Regex::new(r"(?Rm)^ *\(samples \d+ (.*)\)\)$").unwrap();
    // gets us the values portion of (samples n value1 value2 value3...) in the file
    let caps: Vec<&str> = re
        .captures_iter(&text)
        .map(|it| it.get(1).unwrap().as_str())
        .collect();

    // 1 value curve (gray), and 3 colour curves (R, G, B). Possibly also alpha but that is ignored
    assert!(
        caps.len() >= 4,
        "Could not parse 4 curves from file. Exiting..."
    );

    let gray = parse_curve(caps[0]);
    // GIMP doesn't seem to save curves of different accuracy
    assert!(gray.len() == 256);

    let rgb_values = caps[1..4].iter().map(|&list| parse_curve(list));

    rgb_values
        // apply gray curve to each of the RGB curves, reducing 4 curves to 3 colour channel curves
        .map(|color_curve| {
            color_curve
                .iter()
                // lcms2 expects u16, but GIMP only gives us 255 values, so we need to scale down to u8
                .map(|&color_value| gray[scale_u16_to_u8_range(color_value) as usize])
                .collect::<Vec<u16>>()
        })
        .collect::<Vec<Vec<u16>>>()
}

/// Generates a gamma correction curve for the specified gamma value
fn generate_gamma_curve(gamma: f32) -> Vec<Vec<u16>> {
    let single_curve: Vec<u16> = (0..=255)
        .map(|n: u16| {
            let max_value = f32::from(u16::MAX);
            // the exponent here is inversed so that a higher gamma value results in a brighter image
            let multiplier = (f32::from(n) / 255.0).powf(1.0 / gamma);
            (max_value * multiplier) as u16
        })
        .collect();
    vec![
        single_curve.clone(),
        single_curve.clone(),
        single_curve.clone(),
    ]
}

fn main() {
    let args = Arguments::parse();
    let mut icc = Profile::new_srgb();

    icc.remove_tag(lcms2::TagSignature::ProfileDescriptionTag);

    // description that is shown in Windows colour management
    let mut desc = MLU::new(1);
    desc.set_text(&args.description, Locale::none());
    icc.write_tag(lcms2::TagSignature::ProfileDescriptionTag, Tag::MLU(&desc));

    let rgb_curves = if args.input.input_curves.is_some() {
        let input = &args.input.input_curves.unwrap();
        // curves are exported from GIMP curve tool
        println!("reading curve samples from {:?}...", input);
        let text = fs::read_to_string(input)
            .unwrap_or_else(|err| panic!("Could not read file {:?}: {}", input, err));

        let rgb_curves = parse_curves(text);

        // notify user if the curve isn't monotonically increasing
        for (curve_idx, curve) in rgb_curves.iter().enumerate() {
            for ((prev_idx, prev), (next_idx, next)) in curve.iter().enumerate().tuple_windows() {
                if prev > next {
                    println!("curve is not monotonic due to values at indexes {prev_idx} and {next_idx} in {curve_idx}.\nthis may cause shades to look wrong\n")
                }
            }
        }

        rgb_curves
    } else {
        generate_gamma_curve(args.input.gamma.unwrap())
    };

    let r_tc = ToneCurve::new_tabulated(&rgb_curves[0]);
    let g_tc = ToneCurve::new_tabulated(&rgb_curves[1]);
    let b_tc = ToneCurve::new_tabulated(&rgb_curves[2]);

    let tc_refs: [&lcms2::ToneCurveRef; 3] = [&r_tc, &g_tc, &b_tc];
    let vcgt_tag = Tag::VcgtCurves(tc_refs);

    // This tag seems to be just about completely undocumented. It's not a part
    // of the official ICC, even though Windows, MacOS, and Linux use it for
    // calibration.
    // LCMS documentation also does not mention this tag, however,
    // from looking at the source code, it seems this is always 256
    // words, which means we're limited to 8-bit calibration
    icc.write_tag(lcms2::TagSignature::VcgtTag, vcgt_tag);

    println!("saving profile to {:?}...", args.icc_output);
    icc.save_profile_to_file(args.icc_output.as_path())
        .unwrap_or_else(|err| panic!("Error while saving profile to: {err}",));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Tests the entirety of parse_curves() with a known example
    fn parsing_example_input_works() {
        let input = fs::read_to_string("test/gimp_test_curve.txt").unwrap();
        let parsed_result = parse_curves(input);

        // big array
        let expected = vec![
            vec![
                0, 0, 0, 0, 232, 463, 696, 928, 1161, 1395, 1630, 1867, 2104, 2343, 2584, 2826,
                3070, 3317, 3565, 3816, 4070, 4327, 4586, 4848, 5120, 5387, 5655, 5923, 6192, 6461,
                6730, 7000, 7270, 7542, 7813, 8086, 8359, 8634, 8909, 9185, 9462, 9741, 10020,
                10301, 10583, 10866, 11151, 11437, 11725, 12014, 12305, 12597, 12891, 13187, 13485,
                13785, 14087, 14390, 14696, 14848, 15156, 15466, 15776, 16088, 16401, 16714, 17029,
                17345, 17661, 17978, 18296, 18615, 18935, 19255, 19576, 19898, 20221, 20544, 20868,
                21192, 21517, 21843, 22168, 22495, 22822, 23149, 23477, 23805, 24133, 24462, 24791,
                25120, 25449, 25779, 26109, 26439, 26769, 27099, 27429, 27759, 28089, 28420, 28750,
                29080, 29410, 29739, 30069, 30398, 30728, 31056, 31385, 31713, 32041, 32369, 32696,
                33023, 33350, 33675, 34001, 34326, 34650, 34974, 35297, 35619, 35941, 36262, 36583,
                36902, 37221, 37539, 37856, 38172, 38488, 38802, 39116, 39428, 39740, 40051, 40360,
                40668, 40976, 41282, 41587, 41890, 42193, 42494, 42794, 43093, 43390, 43686, 43981,
                44274, 44565, 44856, 45144, 45432, 45717, 46001, 46283, 46564, 46847, 47123, 47394,
                47660, 47921, 48178, 48430, 48678, 48922, 49162, 49398, 49630, 49858, 50083, 50305,
                50524, 50739, 50952, 51161, 51368, 51573, 51775, 51974, 52172, 52367, 52561, 52752,
                52943, 53131, 53318, 53504, 53689, 53873, 54056, 54238, 54420, 54601, 54782, 54963,
                55144, 55324, 55505, 55687, 55868, 56051, 56234, 56418, 56603, 56789, 56976, 57165,
                57355, 57547, 57855, 58050, 58245, 58438, 58631, 58823, 59014, 59205, 59394, 59583,
                59772, 59959, 60146, 60333, 60518, 60703, 60888, 61072, 61255, 61438, 61621, 61803,
                61984, 62166, 62346, 62527, 62707, 62886, 63066, 63245, 63424, 63602, 63780, 63959,
                64136, 64314, 64492, 64669, 64847, 65024, 65201, 65378, 65535,
            ],
            vec![
                0, 0, 0, 0, 232, 463, 696, 928, 1161, 1395, 1630, 1867, 2104, 2343, 2584, 2826,
                3070, 3317, 3565, 3816, 4070, 4327, 4586, 4848, 5120, 5387, 5655, 5923, 6192, 6461,
                6730, 7000, 7270, 7542, 7813, 8086, 8359, 8634, 8909, 9185, 9462, 9741, 10020,
                10301, 10583, 10866, 11151, 11437, 11725, 12014, 12305, 12597, 12891, 13187, 13485,
                13785, 14087, 14390, 14696, 14848, 15156, 15466, 15776, 16088, 16401, 16714, 17029,
                17345, 17661, 17978, 18296, 18615, 18935, 19255, 19576, 19898, 20221, 20544, 20868,
                21192, 21517, 21843, 22168, 22495, 22822, 23149, 23477, 23805, 24133, 24462, 24791,
                25120, 25449, 25779, 26109, 26439, 26769, 27099, 27429, 27759, 28089, 28420, 28750,
                29080, 29410, 29739, 30069, 30398, 30728, 31056, 31385, 31713, 32041, 32369, 32696,
                33023, 33350, 33675, 34001, 34326, 34650, 34974, 35297, 35619, 35941, 36262, 36583,
                36902, 37221, 37539, 37856, 38172, 38488, 38802, 39116, 39428, 39740, 40051, 40360,
                40668, 40976, 41282, 41587, 41890, 42193, 42494, 42794, 43093, 43390, 43686, 43981,
                44274, 44565, 44856, 45144, 45432, 45717, 46001, 46283, 46564, 46847, 47123, 47394,
                47660, 47921, 48178, 48430, 48678, 48922, 49162, 49398, 49630, 49858, 50083, 50305,
                50524, 50739, 50952, 51161, 51368, 51573, 51775, 51974, 52172, 52367, 52561, 52752,
                52943, 53131, 53318, 53504, 53689, 53873, 54056, 54238, 54420, 54601, 54782, 54963,
                55144, 55324, 55505, 55687, 55868, 56051, 56234, 56418, 56603, 56789, 56976, 57165,
                57355, 57547, 57855, 58050, 58245, 58438, 58631, 58823, 59014, 59205, 59394, 59583,
                59772, 59959, 60146, 60333, 60518, 60703, 60888, 61072, 61255, 61438, 61621, 61803,
                61984, 62166, 62346, 62527, 62707, 62886, 63066, 63245, 63424, 63602, 63780, 63959,
                64136, 64314, 64492, 64669, 64847, 65024, 65201, 65378, 65535,
            ],
            vec![
                0, 0, 0, 0, 232, 463, 696, 928, 1161, 1395, 1630, 1867, 2104, 2343, 2584, 2826,
                3070, 3317, 3565, 3816, 4070, 4327, 4586, 4848, 5120, 5387, 5655, 5923, 6192, 6461,
                6730, 7000, 7270, 7542, 7813, 8086, 8359, 8634, 8909, 9185, 9462, 9741, 10020,
                10301, 10583, 10866, 11151, 11437, 11725, 12014, 12305, 12597, 12891, 13187, 13485,
                13785, 14087, 14390, 14696, 14848, 15156, 15466, 15776, 16088, 16401, 16714, 17029,
                17345, 17661, 17978, 18296, 18615, 18935, 19255, 19576, 19898, 20221, 20544, 20868,
                21192, 21517, 21843, 22168, 22495, 22822, 23149, 23477, 23805, 24133, 24462, 24791,
                25120, 25449, 25779, 26109, 26439, 26769, 27099, 27429, 27759, 28089, 28420, 28750,
                29080, 29410, 29739, 30069, 30398, 30728, 31056, 31385, 31713, 32041, 32369, 32696,
                33023, 33350, 33675, 34001, 34326, 34650, 34974, 35297, 35619, 35941, 36262, 36583,
                36902, 37221, 37539, 37856, 38172, 38488, 38802, 39116, 39428, 39740, 40051, 40360,
                40668, 40976, 41282, 41587, 41890, 42193, 42494, 42794, 43093, 43390, 43686, 43981,
                44274, 44565, 44856, 45144, 45432, 45717, 46001, 46283, 46564, 46847, 47123, 47394,
                47660, 47921, 48178, 48430, 48678, 48922, 49162, 49398, 49630, 49858, 50083, 50305,
                50524, 50739, 50952, 51161, 51368, 51573, 51775, 51974, 52172, 52367, 52561, 52752,
                52943, 53131, 53318, 53504, 53689, 53873, 54056, 54238, 54420, 54601, 54782, 54963,
                55144, 55324, 55505, 55687, 55868, 56051, 56234, 56418, 56603, 56789, 56976, 57165,
                57355, 57547, 57855, 58050, 58245, 58438, 58631, 58823, 59014, 59205, 59394, 59583,
                59772, 59959, 60146, 60333, 60518, 60703, 60888, 61072, 61255, 61438, 61621, 61803,
                61984, 62166, 62346, 62527, 62707, 62886, 63066, 63245, 63424, 63602, 63780, 63959,
                64136, 64314, 64492, 64669, 64847, 65024, 65201, 65378, 65535,
            ],
        ];

        assert_eq!(parsed_result, expected);
    }

    #[test]
    fn bright_gamma_ramp_works() {
        let gamma = 1.25;
        let result = generate_gamma_curve(gamma);

        let expected_single_curve = vec![
            0, 778, 1355, 1874, 2359, 2821, 3264, 3692, 4108, 4514, 4911, 5300, 5683, 6058, 6429,
            6793, 7153, 7509, 7860, 8208, 8551, 8892, 9229, 9563, 9894, 10223, 10549, 10872, 11193,
            11512, 11828, 12143, 12455, 12765, 13074, 13381, 13686, 13989, 14291, 14591, 14889,
            15186, 15482, 15776, 16069, 16361, 16651, 16940, 17227, 17514, 17799, 18084, 18367,
            18649, 18930, 19210, 19489, 19767, 20043, 20319, 20595, 20869, 21142, 21414, 21686,
            21956, 22226, 22495, 22763, 23031, 23298, 23563, 23829, 24093, 24357, 24620, 24882,
            25143, 25404, 25665, 25924, 26183, 26441, 26699, 26956, 27212, 27468, 27724, 27978,
            28232, 28486, 28739, 28991, 29243, 29494, 29745, 29995, 30245, 30494, 30743, 30991,
            31239, 31486, 31732, 31979, 32224, 32470, 32715, 32959, 33203, 33446, 33689, 33932,
            34174, 34416, 34657, 34898, 35139, 35379, 35618, 35858, 36096, 36335, 36573, 36811,
            37048, 37285, 37521, 37758, 37993, 38229, 38464, 38699, 38933, 39167, 39401, 39634,
            39867, 40100, 40332, 40564, 40795, 41027, 41258, 41488, 41719, 41949, 42178, 42408,
            42637, 42866, 43094, 43322, 43550, 43778, 44005, 44232, 44459, 44685, 44911, 45137,
            45363, 45588, 45813, 46037, 46262, 46486, 46710, 46934, 47157, 47380, 47603, 47825,
            48048, 48270, 48492, 48713, 48935, 49156, 49376, 49597, 49817, 50037, 50257, 50477,
            50696, 50915, 51134, 51353, 51571, 51789, 52007, 52225, 52442, 52660, 52877, 53094,
            53310, 53527, 53743, 53959, 54174, 54390, 54605, 54820, 55035, 55250, 55464, 55679,
            55893, 56106, 56320, 56534, 56747, 56960, 57173, 57385, 57598, 57810, 58022, 58234,
            58446, 58657, 58868, 59079, 59290, 59501, 59712, 59922, 60132, 60342, 60552, 60761,
            60971, 61180, 61389, 61598, 61807, 62015, 62224, 62432, 62640, 62848, 63055, 63263,
            63470, 63677, 63884, 64091, 64298, 64504, 64711, 64917, 65123, 65329, 65535,
        ];
        let expected = vec![
            expected_single_curve.clone(),
            expected_single_curve.clone(),
            expected_single_curve.clone(),
        ];

        assert_eq!(result, expected);
    }

    #[test]
    fn dark_gamma_ramp_works() {
        let gamma = 0.75;
        let result = generate_gamma_curve(gamma);

        let expected_single_curve = vec![
            0, 40, 102, 175, 257, 346, 441, 542, 648, 758, 873, 991, 1113, 1238, 1367, 1499, 1633,
            1771, 1911, 2054, 2200, 2348, 2498, 2650, 2805, 2962, 3121, 3282, 3445, 3610, 3777,
            3946, 4117, 4289, 4464, 4639, 4817, 4996, 5177, 5360, 5544, 5729, 5916, 6105, 6295,
            6486, 6679, 6874, 7069, 7266, 7465, 7665, 7866, 8068, 8272, 8476, 8683, 8890, 9098,
            9308, 9519, 9731, 9945, 10159, 10375, 10591, 10809, 11028, 11248, 11469, 11691, 11915,
            12139, 12364, 12591, 12818, 13046, 13276, 13506, 13737, 13970, 14203, 14437, 14673,
            14909, 15146, 15384, 15623, 15863, 16104, 16345, 16588, 16832, 17076, 17321, 17567,
            17814, 18062, 18311, 18560, 18811, 19062, 19314, 19567, 19821, 20075, 20331, 20587,
            20844, 21101, 21360, 21619, 21879, 22140, 22402, 22664, 22927, 23191, 23456, 23721,
            23988, 24254, 24522, 24790, 25060, 25329, 25600, 25871, 26143, 26416, 26689, 26963,
            27238, 27514, 27790, 28067, 28344, 28622, 28901, 29181, 29461, 29742, 30024, 30306,
            30589, 30872, 31157, 31441, 31727, 32013, 32300, 32587, 32875, 33164, 33453, 33743,
            34034, 34325, 34617, 34909, 35203, 35496, 35790, 36085, 36381, 36677, 36974, 37271,
            37569, 37867, 38166, 38466, 38766, 39067, 39368, 39670, 39973, 40276, 40580, 40884,
            41189, 41494, 41800, 42107, 42414, 42721, 43029, 43338, 43647, 43957, 44268, 44579,
            44890, 45202, 45515, 45828, 46141, 46455, 46770, 47085, 47401, 47717, 48034, 48352,
            48669, 48988, 49307, 49626, 49946, 50266, 50587, 50909, 51231, 51553, 51876, 52200,
            52524, 52848, 53173, 53498, 53824, 54151, 54478, 54805, 55133, 55462, 55791, 56120,
            56450, 56780, 57111, 57442, 57774, 58106, 58439, 58772, 59106, 59440, 59775, 60110,
            60446, 60782, 61118, 61455, 61793, 62130, 62469, 62808, 63147, 63487, 63827, 64167,
            64509, 64850, 65192, 65535,
        ];
        let expected = vec![
            expected_single_curve.clone(),
            expected_single_curve.clone(),
            expected_single_curve.clone(),
        ];

        assert_eq!(result, expected);
    }
}
