import { Button, Picker, Text, Textarea, View } from '@tarojs/components'
import { useController } from '../../runtime'
import { Notice, PageHeader } from '../../runtime/ui'
import controller from './fault.controller'
import './fault.css'

export default function PageView() {
  const { data, event } = useController(controller)
  const { deviceId, faultTypeIndex, faultTypes, description, submitting, needsLogin, error, historyError, notice, reports, reportsHasMore, loadingReports, expandedReport, reportHistory, reportHistoryLoading, reportHistoryError } = data
  return <View className='container'>
    <PageHeader title='设备报修' subtitle='请勿填写个人密码或支付信息。' />
    {needsLogin && <View className='hero'><View className='hero-body'>
      <Text className='hero-title'>登录后提交设备报修</Text>
      <Button type='primary' onClick={event('login', {})}>微信登录</Button>
    </View></View>}
    <View className='card'>
      <Text className='heading'>故障描述</Text>
      <View className='device-chip'>设备编号：{deviceId || '未指定'}</View>
      {!deviceId && <Button onClick={event('scanDevice')}>扫码选择报修设备</Button>}
      <Text className='field-label'>故障类型</Text>
      <Picker range={faultTypes} value={faultTypeIndex} onChange={event('onType', {})}><View className='field'>故障类型：{faultTypes?.[faultTypeIndex]} ▾</View></Picker>
      <Text className='field-label'>现象说明</Text>
      <Textarea placeholder='描述故障现象、发生时间和已尝试的处理方式' maxlength={2000} value={description} onInput={event('onDescription', {})} autoHeight />
      <Button type='primary' onClick={event('submit', {})} loading={submitting} disabled={submitting}>提交报修</Button>
    </View>
    {notice && <View className='banner banner-ok'>{notice}</View>}
    {error && <Notice tone='error' retry={event('submit', {})} busy={submitting}>{error}</Notice>}
    <View className='card'>
      <Text className='heading'>我的报修</Text>
      {(reports || []).map((item: any, index: number) => <View className='report-row' key={String(item?.report_id ?? index)}>
        <View className='report-head'><Text>{item?.device_id}</Text><Text className='tag tag-green'>{item?.statusText}</Text></View>
        <Text className='text-secondary'>报修单 {item?.report_id} · {item?.createdText}</Text>
        {item?.description && <Text className='report-description'>{item.description}</Text>}
        <Button className='history-toggle' size='mini' data-report-id={item?.report_id} onClick={event('toggleHistory', { 'report-id': item?.report_id })}>{expandedReport === item?.report_id ? '收起处理记录' : '查看处理记录'}</Button>
        {expandedReport === item?.report_id && <View className='history-box'>
          {reportHistoryLoading && <Text className='text-secondary'>正在读取处理记录…</Text>}
          {(reportHistory || []).map((entry: any, entryIndex: number) => <View className='history-row' key={String(entry?.event_id ?? entryIndex)}>
            <Text>{entry?.eventText} · {entry?.createdText}</Text>
            {(entry?.fromStatusText || entry?.toStatusText) && <Text className='text-secondary'>{entry?.fromStatusText || '—'} → {entry?.toStatusText || '—'}</Text>}
            {entry?.note && <Text className='history-note'>{entry.note}</Text>}
          </View>)}
          {reportHistoryError && <View className='error'><Text>{reportHistoryError}</Text><Button size='mini' data-report-id={item?.report_id} onClick={event('retryReportHistory', { 'report-id': item?.report_id })}>重试</Button></View>}
          {!reportHistoryLoading && !reportHistoryError && reportHistory?.length === 0 && <Text className='text-secondary'>暂无处理记录</Text>}
        </View>}
      </View>)}
      {reports?.length === 0 && !loadingReports && <Text className='text-secondary'>暂无报修记录</Text>}
      {historyError && <Notice tone='error' retry={event('retryReports', {})} busy={loadingReports}>{historyError}</Notice>}
      {reportsHasMore && <Button onClick={event('loadMoreReports', {})} loading={loadingReports}>加载更多</Button>}
      {loadingReports && <Text className='text-secondary'>正在读取报修进度…</Text>}
    </View>
  </View>
}
