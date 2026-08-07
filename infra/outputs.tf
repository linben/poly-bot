output "dashboard_url" {
  value = "https://${aws_cloudfront_distribution.dashboard.domain_name}/"
}

output "api_endpoint" {
  value = aws_apigatewayv2_api.dashboard.api_endpoint
}

output "cognito_user_pool_id" {
  value = aws_cognito_user_pool.dashboard.id
}

output "application_secret_id" {
  value = aws_secretsmanager_secret.app.id
}

output "scanner_repository_url" {
  value = aws_ecr_repository.scanner.repository_url
}

output "data_bucket" {
  value = aws_s3_bucket.data.id
}
