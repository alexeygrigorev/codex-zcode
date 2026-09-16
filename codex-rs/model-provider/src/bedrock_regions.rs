/// Amazon Bedrock Mantle regions that do not require the optional `bedrock`
/// feature. Kept outside the feature-gated `amazon_bedrock` module so callers
/// such as app-server can validate regions unconditionally.
const BEDROCK_MANTLE_SUPPORTED_REGIONS: [&str; 12] = [
    "us-east-2",
    "us-east-1",
    "us-west-2",
    "ap-southeast-3",
    "ap-south-1",
    "ap-northeast-1",
    "eu-central-1",
    "eu-west-1",
    "eu-west-2",
    "eu-south-1",
    "eu-north-1",
    "sa-east-1",
];

/// Returns whether Amazon Bedrock Mantle is available in `region`.
pub fn is_supported_amazon_bedrock_region(region: &str) -> bool {
    BEDROCK_MANTLE_SUPPORTED_REGIONS.contains(&region)
}
