variable "project_name" {
  type    = string
  default = "polybot"
}

variable "aws_region" {
  type    = string
  default = "us-east-1"
}

variable "scanner_image_tag" {
  description = "Immutable scanner image tag, normally the Git commit SHA."
  type        = string
}

variable "api_lambda_zip" {
  description = "Path to the cargo-lambda API zip."
  type        = string
  default     = "../target/lambda/api/bootstrap.zip"
}

variable "news_lambda_zip" {
  description = "Path to the cargo-lambda news-worker zip."
  type        = string
  default     = "../target/lambda/news-worker/bootstrap.zip"
}

variable "lambda_architecture" {
  description = "Architecture emitted by cargo-lambda."
  type        = string
  default     = "arm64"

  validation {
    condition     = contains(["arm64", "x86_64"], var.lambda_architecture)
    error_message = "lambda_architecture must be arm64 or x86_64."
  }
}

variable "bedrock_model_id" {
  description = "Bedrock Anthropic model or inference-profile ID."
  type        = string
}

variable "source_endpoints" {
  description = "Feasibility-approved canonical source endpoint URLs keyed by source ID."
  type        = map(string)
  default     = {}
}

variable "cognito_domain_prefix" {
  type    = string
  default = "polybot-research"
}
