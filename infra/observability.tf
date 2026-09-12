resource "aws_cloudwatch_metric_alarm" "news_errors" {
  alarm_name          = "${var.project_name}-news-errors"
  namespace           = "AWS/Lambda"
  metric_name         = "Errors"
  statistic           = "Sum"
  period              = 300
  evaluation_periods  = 1
  threshold           = 0
  comparison_operator = "GreaterThanThreshold"
  treat_missing_data  = "notBreaching"
  dimensions = {
    FunctionName = aws_lambda_function.news.function_name
  }
}

resource "aws_cloudwatch_metric_alarm" "news_dlq" {
  alarm_name          = "${var.project_name}-news-dlq"
  namespace           = "AWS/SQS"
  metric_name         = "ApproximateNumberOfMessagesVisible"
  statistic           = "Maximum"
  period              = 300
  evaluation_periods  = 1
  threshold           = 0
  comparison_operator = "GreaterThanThreshold"
  treat_missing_data  = "notBreaching"
  dimensions = {
    QueueName = aws_sqs_queue.news_dlq.name
  }
}

resource "aws_cloudwatch_metric_alarm" "scanner_schedule_errors" {
  alarm_name          = "${var.project_name}-scanner-schedule-errors"
  namespace           = "AWS/Scheduler"
  metric_name         = "TargetErrorCount"
  statistic           = "Sum"
  period              = 300
  evaluation_periods  = 1
  threshold           = 0
  comparison_operator = "GreaterThanThreshold"
  treat_missing_data  = "notBreaching"
  dimensions = {
    ScheduleGroup = "default"
  }
}

resource "aws_cloudwatch_log_metric_filter" "scanner_failures" {
  name           = "${var.project_name}-scanner-failures"
  log_group_name = aws_cloudwatch_log_group.scanner.name
  pattern        = "\"scan failed\""

  metric_transformation {
    name      = "ScannerFailures"
    namespace = "Polybot"
    value     = "1"
  }
}

resource "aws_cloudwatch_metric_alarm" "scanner_failures" {
  alarm_name          = "${var.project_name}-scanner-failures"
  namespace           = "Polybot"
  metric_name         = "ScannerFailures"
  statistic           = "Sum"
  period              = 300
  evaluation_periods  = 1
  threshold           = 0
  comparison_operator = "GreaterThanThreshold"
  treat_missing_data  = "notBreaching"
}
