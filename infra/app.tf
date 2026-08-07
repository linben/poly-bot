data "aws_iam_policy_document" "lambda_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "api" {
  name_prefix        = "${var.project_name}-api-"
  assume_role_policy = data.aws_iam_policy_document.lambda_assume.json
}

resource "aws_iam_role_policy_attachment" "api_logs" {
  role       = aws_iam_role.api.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_iam_role_policy" "api" {
  role = aws_iam_role.api.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = ["dynamodb:GetItem"]
      Resource = aws_dynamodb_table.state.arn
    }]
  })
}

resource "aws_cloudwatch_log_group" "api" {
  name              = "/aws/lambda/${var.project_name}-api"
  retention_in_days = 30
}

resource "aws_lambda_function" "api" {
  function_name    = "${var.project_name}-api"
  filename         = var.api_lambda_zip
  role             = aws_iam_role.api.arn
  handler          = "bootstrap"
  runtime          = "provided.al2023"
  architectures    = [var.lambda_architecture]
  timeout          = 15
  memory_size      = 256
  source_code_hash = filebase64sha256(var.api_lambda_zip)

  environment {
    variables = {
      DATA_BUCKET    = aws_s3_bucket.data.id
      STATE_TABLE    = aws_dynamodb_table.state.name
      NEWS_QUEUE_URL = aws_sqs_queue.news.url
      RUST_LOG       = "polybot=info"
    }
  }

  depends_on = [
    aws_cloudwatch_log_group.api,
    aws_iam_role_policy_attachment.api_logs,
  ]
}

resource "aws_iam_role" "news" {
  name_prefix        = "${var.project_name}-news-"
  assume_role_policy = data.aws_iam_policy_document.lambda_assume.json
}

resource "aws_iam_role_policy_attachment" "news_logs" {
  role       = aws_iam_role.news.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_iam_role_policy" "news" {
  role = aws_iam_role.news.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["dynamodb:PutItem"]
        Resource = aws_dynamodb_table.state.arn
      },
      {
        Effect   = "Allow"
        Action   = ["bedrock:InvokeModel"]
        Resource = "*"
      },
      {
        Effect   = "Allow"
        Action   = ["secretsmanager:GetSecretValue"]
        Resource = aws_secretsmanager_secret.app.arn
      },
      {
        Effect = "Allow"
        Action = [
          "sqs:ReceiveMessage",
          "sqs:DeleteMessage",
          "sqs:GetQueueAttributes"
        ]
        Resource = aws_sqs_queue.news.arn
      }
    ]
  })
}

resource "aws_cloudwatch_log_group" "news" {
  name              = "/aws/lambda/${var.project_name}-news-worker"
  retention_in_days = 30
}

resource "aws_lambda_function" "news" {
  function_name    = "${var.project_name}-news-worker"
  filename         = var.news_lambda_zip
  role             = aws_iam_role.news.arn
  handler          = "bootstrap"
  runtime          = "provided.al2023"
  architectures    = [var.lambda_architecture]
  timeout          = 120
  memory_size      = 512
  source_code_hash = filebase64sha256(var.news_lambda_zip)

  environment {
    variables = {
      APP_SECRET_ID        = aws_secretsmanager_secret.app.id
      BEDROCK_MODEL_ID     = var.bedrock_model_id
      DATA_BUCKET          = aws_s3_bucket.data.id
      STATE_TABLE          = aws_dynamodb_table.state.name
      NEWS_QUEUE_URL       = aws_sqs_queue.news.url
      NEWS_REFRESH_SECONDS = "3600"
      RUST_LOG             = "polybot=info"
    }
  }

  depends_on = [
    aws_cloudwatch_log_group.news,
    aws_iam_role_policy_attachment.news_logs,
  ]
}

resource "aws_lambda_event_source_mapping" "news" {
  event_source_arn                   = aws_sqs_queue.news.arn
  function_name                      = aws_lambda_function.news.arn
  batch_size                         = 5
  maximum_batching_window_in_seconds = 5
  function_response_types            = ["ReportBatchItemFailures"]
}

resource "aws_cognito_user_pool" "dashboard" {
  name                     = "${var.project_name}-dashboard"
  username_attributes      = ["email"]
  auto_verified_attributes = ["email"]

  admin_create_user_config {
    allow_admin_create_user_only = true
  }

  password_policy {
    minimum_length                   = 12
    require_lowercase                = true
    require_numbers                  = true
    require_symbols                  = true
    require_uppercase                = true
    temporary_password_validity_days = 7
  }
}

resource "aws_cognito_user_pool_domain" "dashboard" {
  domain       = "${var.cognito_domain_prefix}-${data.aws_caller_identity.current.account_id}"
  user_pool_id = aws_cognito_user_pool.dashboard.id
}

resource "aws_apigatewayv2_api" "dashboard" {
  name          = "${var.project_name}-dashboard"
  protocol_type = "HTTP"
}

resource "aws_apigatewayv2_integration" "api" {
  api_id                 = aws_apigatewayv2_api.dashboard.id
  integration_type       = "AWS_PROXY"
  integration_uri        = aws_lambda_function.api.invoke_arn
  payload_format_version = "2.0"
  timeout_milliseconds   = 15000
}

resource "aws_cognito_user_pool_client" "dashboard" {
  name                                 = "${var.project_name}-dashboard"
  user_pool_id                         = aws_cognito_user_pool.dashboard.id
  generate_secret                      = false
  allowed_oauth_flows_user_pool_client = true
  allowed_oauth_flows                  = ["code"]
  allowed_oauth_scopes                 = ["openid", "email"]
  callback_urls                        = ["https://${aws_cloudfront_distribution.dashboard.domain_name}/"]
  logout_urls                          = ["https://${aws_cloudfront_distribution.dashboard.domain_name}/"]
  supported_identity_providers         = ["COGNITO"]
  prevent_user_existence_errors        = "ENABLED"
}

resource "aws_apigatewayv2_authorizer" "dashboard" {
  api_id           = aws_apigatewayv2_api.dashboard.id
  authorizer_type  = "JWT"
  identity_sources = ["$request.header.Authorization"]
  name             = "${var.project_name}-cognito"

  jwt_configuration {
    audience = [aws_cognito_user_pool_client.dashboard.id]
    issuer   = "https://cognito-idp.${var.aws_region}.amazonaws.com/${aws_cognito_user_pool.dashboard.id}"
  }
}

resource "aws_apigatewayv2_route" "opportunities" {
  api_id             = aws_apigatewayv2_api.dashboard.id
  route_key          = "GET /api/opportunities"
  target             = "integrations/${aws_apigatewayv2_integration.api.id}"
  authorization_type = "JWT"
  authorizer_id      = aws_apigatewayv2_authorizer.dashboard.id
}

resource "aws_apigatewayv2_route" "health" {
  api_id    = aws_apigatewayv2_api.dashboard.id
  route_key = "GET /health"
  target    = "integrations/${aws_apigatewayv2_integration.api.id}"
}

resource "aws_apigatewayv2_stage" "dashboard" {
  api_id      = aws_apigatewayv2_api.dashboard.id
  name        = "$default"
  auto_deploy = true

  default_route_settings {
    detailed_metrics_enabled = true
    throttling_burst_limit   = 20
    throttling_rate_limit    = 10
  }
}

resource "aws_lambda_permission" "api_gateway" {
  statement_id  = "AllowApiGateway"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.api.function_name
  principal     = "apigateway.amazonaws.com"
  source_arn    = "${aws_apigatewayv2_api.dashboard.execution_arn}/*/*"
}

resource "aws_s3_bucket" "dashboard" {
  bucket_prefix = "${var.project_name}-dashboard-"
}

resource "aws_s3_bucket_server_side_encryption_configuration" "dashboard" {
  bucket = aws_s3_bucket.dashboard.id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

resource "aws_s3_bucket_public_access_block" "dashboard" {
  bucket                  = aws_s3_bucket.dashboard.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_cloudfront_origin_access_control" "dashboard" {
  name                              = "${var.project_name}-dashboard"
  description                       = "Private dashboard bucket access"
  origin_access_control_origin_type = "s3"
  signing_behavior                  = "always"
  signing_protocol                  = "sigv4"
}

data "aws_cloudfront_cache_policy" "disabled" {
  name = "Managed-CachingDisabled"
}

data "aws_cloudfront_cache_policy" "optimized" {
  name = "Managed-CachingOptimized"
}

data "aws_cloudfront_origin_request_policy" "api" {
  name = "Managed-AllViewerExceptHostHeader"
}

resource "aws_cloudfront_distribution" "dashboard" {
  enabled             = true
  default_root_object = "index.html"
  price_class         = "PriceClass_100"

  origin {
    domain_name              = aws_s3_bucket.dashboard.bucket_regional_domain_name
    origin_id                = "dashboard-s3"
    origin_access_control_id = aws_cloudfront_origin_access_control.dashboard.id
  }

  origin {
    domain_name = replace(aws_apigatewayv2_api.dashboard.api_endpoint, "https://", "")
    origin_id   = "dashboard-api"
    custom_origin_config {
      http_port              = 80
      https_port             = 443
      origin_protocol_policy = "https-only"
      origin_ssl_protocols   = ["TLSv1.2"]
    }
  }

  default_cache_behavior {
    target_origin_id       = "dashboard-s3"
    viewer_protocol_policy = "redirect-to-https"
    allowed_methods        = ["GET", "HEAD", "OPTIONS"]
    cached_methods         = ["GET", "HEAD"]
    cache_policy_id        = data.aws_cloudfront_cache_policy.optimized.id
    compress               = true
  }

  ordered_cache_behavior {
    path_pattern             = "/api/*"
    target_origin_id         = "dashboard-api"
    viewer_protocol_policy   = "https-only"
    allowed_methods          = ["GET", "HEAD", "OPTIONS"]
    cached_methods           = ["GET", "HEAD"]
    cache_policy_id          = data.aws_cloudfront_cache_policy.disabled.id
    origin_request_policy_id = data.aws_cloudfront_origin_request_policy.api.id
    compress                 = true
  }

  ordered_cache_behavior {
    path_pattern             = "/health"
    target_origin_id         = "dashboard-api"
    viewer_protocol_policy   = "https-only"
    allowed_methods          = ["GET", "HEAD", "OPTIONS"]
    cached_methods           = ["GET", "HEAD"]
    cache_policy_id          = data.aws_cloudfront_cache_policy.disabled.id
    origin_request_policy_id = data.aws_cloudfront_origin_request_policy.api.id
    compress                 = true
  }

  restrictions {
    geo_restriction {
      restriction_type = "none"
    }
  }

  viewer_certificate {
    cloudfront_default_certificate = true
    minimum_protocol_version       = "TLSv1.2_2021"
  }
}

data "aws_iam_policy_document" "dashboard_bucket" {
  statement {
    actions   = ["s3:GetObject"]
    resources = ["${aws_s3_bucket.dashboard.arn}/*"]
    principals {
      type        = "Service"
      identifiers = ["cloudfront.amazonaws.com"]
    }
    condition {
      test     = "StringEquals"
      variable = "AWS:SourceArn"
      values   = [aws_cloudfront_distribution.dashboard.arn]
    }
  }
}

resource "aws_s3_bucket_policy" "dashboard" {
  bucket = aws_s3_bucket.dashboard.id
  policy = data.aws_iam_policy_document.dashboard_bucket.json
}

resource "aws_s3_object" "dashboard_index" {
  bucket        = aws_s3_bucket.dashboard.id
  key           = "index.html"
  source        = "${path.module}/../web/index.html"
  content_type  = "text/html; charset=utf-8"
  cache_control = "no-cache"
  etag          = filemd5("${path.module}/../web/index.html")
}

locals {
  dashboard_redirect_uri = "https://${aws_cloudfront_distribution.dashboard.domain_name}/"
  dashboard_auth_base    = "https://${aws_cognito_user_pool_domain.dashboard.domain}.auth.${var.aws_region}.amazoncognito.com"
}

resource "aws_s3_object" "dashboard_config" {
  bucket        = aws_s3_bucket.dashboard.id
  key           = "config.js"
  content_type  = "application/javascript"
  cache_control = "no-cache"
  content = "window.POLYBOT_CONFIG = ${jsonencode({
    authorizeUrl = "${local.dashboard_auth_base}/oauth2/authorize"
    tokenUrl     = "${local.dashboard_auth_base}/oauth2/token"
    logoutUrl    = "${local.dashboard_auth_base}/logout?client_id=${aws_cognito_user_pool_client.dashboard.id}&logout_uri=${urlencode(local.dashboard_redirect_uri)}"
    clientId     = aws_cognito_user_pool_client.dashboard.id
    redirectUri  = local.dashboard_redirect_uri
  })};"
}
